//! The Servers view: every Host and remote desktop of the current workspace in a
//! virtualized list or card grid, with reachability, OS, tags, environment, last
//! connection, search, grouping, recents and one-click SSH, SFTP, RDP and VNC.
//!
//! The rows are built by pure functions (profiles, facts, query and grouping in, rows
//! out) so they are unit tested; the drawing lives in `impl Workspace`.

use std::collections::HashMap;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    MouseButton, MouseDownEvent, ParentElement as _, SharedString, StatefulInteractiveElement as _,
    Styled as _, div, px, uniform_list,
};
use switchyard_core::Command;
use switchyard_core::servers::{REACH_SETTING, ReachStatus};
use switchyard_core::store::{
    DesktopProtocol, EnvironmentLabel, Profile, ProfileId, ServerFacts, tags::normalize_tags,
};

use crate::actions::fuzzy_score;
use crate::app_state::Profiles;
use crate::appearance::{rpx, ts};
use crate::server_workspaces::ServerGroup;
use crate::sidebar::{CtxMenu, CtxTarget};
use crate::theme::{MONO, Palette};
use crate::ui::{self, Kind};
use crate::workspace::Workspace;

/// Cards per row of the card grid.
const CARDS_PER_ROW: usize = 3;
/// Recently connected servers shown above the list.
const RECENTS: usize = 5;

/// What a server row opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServerKind {
    /// An SSH Host (terminal, files).
    Host,
    /// A remote desktop.
    Desktop(DesktopProtocol),
}

/// One server.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ServerItem {
    pub id: ProfileId,
    pub name: String,
    pub kind: ServerKind,
    /// `address[:port]`, or `address via <Host>`.
    pub address: String,
    pub os: Option<String>,
    pub tags: Vec<String>,
    pub env: EnvironmentLabel,
    pub folder: Option<String>,
    /// Last connection, ms since the epoch.
    pub last: Option<i64>,
}

/// A row of the list.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ServerRow {
    /// A group heading (label, count).
    Group(String, usize),
    /// A server.
    Item(ServerItem),
}

/// Every server of `profiles` (Hosts and remote desktops), in saved order.
pub(crate) fn server_items(
    profiles: &Profiles,
    facts: &HashMap<ProfileId, ServerFacts>,
) -> Vec<ServerItem> {
    let host_name = |id: &ProfileId| profiles.host(id).map_or("Host", |h| h.name.as_str());
    profiles
        .all
        .iter()
        .filter_map(|p| {
            let f = facts.get(p.id());
            let item = match p {
                Profile::Host(h) => ServerItem {
                    id: h.id.clone(),
                    name: h.name.clone(),
                    kind: ServerKind::Host,
                    address: if h.port == 22 {
                        h.address.clone()
                    } else {
                        format!("{}:{}", h.address, h.port)
                    },
                    os: f.and_then(|f| f.os.clone()),
                    tags: normalize_tags(&h.tags),
                    env: h.environment,
                    folder: h.folder.clone().filter(|f| !f.trim().is_empty()),
                    last: f.and_then(|f| f.last_connected),
                },
                Profile::Desktop(d) => ServerItem {
                    id: d.id.clone(),
                    name: d.name.clone(),
                    kind: ServerKind::Desktop(d.protocol),
                    address: {
                        let base = if d.port == d.protocol.default_port() {
                            d.address.clone()
                        } else {
                            format!("{}:{}", d.address, d.port)
                        };
                        match &d.via_host {
                            Some(h) => format!("{base} via {}", host_name(h)),
                            None => base,
                        }
                    },
                    os: f.and_then(|f| f.os.clone()).or_else(|| d.os.clone()),
                    tags: normalize_tags(&d.tags),
                    env: d.environment,
                    folder: d.folder.clone().filter(|f| !f.trim().is_empty()),
                    last: f.and_then(|f| f.last_connected),
                },
                _ => return None,
            };
            Some(item)
        })
        .collect()
}

/// Production first, Local last.
fn env_rank(e: EnvironmentLabel) -> u8 {
    match e {
        EnvironmentLabel::Production => 0,
        EnvironmentLabel::Staging => 1,
        EnvironmentLabel::Development => 2,
        EnvironmentLabel::Local => 3,
    }
}

/// The rows for `query` (name, address, tag, OS; best match first) and `group`.
pub(crate) fn server_rows(items: &[ServerItem], query: &str, group: ServerGroup) -> Vec<ServerRow> {
    let q = query.trim();
    let tag_only = q.strip_prefix("tag:").map(|t| t.trim().to_lowercase());
    let mut hits: Vec<(usize, &ServerItem)> = items
        .iter()
        .filter_map(|it| match &tag_only {
            Some(t) => it
                .tags
                .iter()
                .any(|x| x == t || (t.ends_with(':') && x.starts_with(t.as_str())))
                .then_some((0, it)),
            None => {
                let hay = format!(
                    "{} {} {} {}",
                    it.name,
                    it.address,
                    it.tags.join(" "),
                    it.os.as_deref().unwrap_or("")
                );
                fuzzy_score(q, &hay).map(|s| (s, it))
            }
        })
        .collect();
    if !q.is_empty() {
        hits.sort_by_key(|(s, _)| *s);
    }
    let list: Vec<&ServerItem> = hits.into_iter().map(|(_, it)| it).collect();
    let mut groups: Vec<(String, Vec<&ServerItem>)> = Vec::new();
    fn push<'a>(groups: &mut Vec<(String, Vec<&'a ServerItem>)>, key: String, it: &'a ServerItem) {
        match groups.iter_mut().find(|(g, _)| *g == key) {
            Some((_, l)) => l.push(it),
            None => groups.push((key, vec![it])),
        }
    }
    match group {
        ServerGroup::None => return list.into_iter().cloned().map(ServerRow::Item).collect(),
        ServerGroup::Folder => {
            for it in &list {
                push(&mut groups, it.folder.clone().unwrap_or_default(), it);
            }
            groups.sort_by(|a, b| (a.0.is_empty(), &a.0).cmp(&(b.0.is_empty(), &b.0)));
        }
        ServerGroup::Tag => {
            for it in &list {
                if it.tags.is_empty() {
                    push(&mut groups, String::new(), it);
                }
                for t in &it.tags {
                    push(&mut groups, t.clone(), it);
                }
            }
            groups.sort_by(|a, b| (a.0.is_empty(), &a.0).cmp(&(b.0.is_empty(), &b.0)));
        }
        ServerGroup::Environment => {
            let mut envs: Vec<EnvironmentLabel> = list.iter().map(|it| it.env).collect();
            envs.sort_by_key(|e| env_rank(*e));
            envs.dedup();
            for e in envs {
                for it in list.iter().filter(|it| it.env == e) {
                    push(&mut groups, e.name().to_owned(), it);
                }
            }
        }
    }
    let empty = match group {
        ServerGroup::Folder => "No folder",
        ServerGroup::Tag => "Untagged",
        _ => "",
    };
    let mut rows = Vec::new();
    for (label, members) in groups {
        let label = if label.is_empty() {
            empty.to_owned()
        } else if group == ServerGroup::Tag {
            format!("# {label}")
        } else {
            label
        };
        rows.push(ServerRow::Group(label, members.len()));
        rows.extend(members.into_iter().cloned().map(ServerRow::Item));
    }
    rows
}

/// The servers connected to most recently, newest first.
pub(crate) fn recent_items(items: &[ServerItem], n: usize) -> Vec<ServerItem> {
    let mut list: Vec<&ServerItem> = items.iter().filter(|it| it.last.is_some()).collect();
    list.sort_by_key(|it| std::cmp::Reverse(it.last));
    list.into_iter().take(n).cloned().collect()
}

/// "5 min ago", "3 h ago", "2 d ago", "never".
pub(crate) fn ago(ms: Option<i64>, now: i64) -> String {
    let Some(ms) = ms else {
        return "never".into();
    };
    let s = (now - ms).max(0) / 1000;
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86_400 => format!("{} h ago", s / 3600),
        _ => format!("{} d ago", s / 86_400),
    }
}

impl Workspace {
    /// The search field, created on first use (it needs the window).
    pub(crate) fn servers_search(
        &mut self,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Entity<InputState> {
        if let Some(s) = &self.servers.view.search {
            return s.clone();
        }
        let s = cx.new(|cx| InputState::new(window, cx).placeholder("Search name, IP, tag:web"));
        let sub = cx.subscribe(&s, |_, _, ev: &InputEvent, cx| {
            if let InputEvent::Change = ev {
                cx.notify();
            }
        });
        self._subs.push(sub);
        self.servers.view.search = Some(s.clone());
        s
    }

    /// Turn the reachability checks on or off (and remember it).
    pub(crate) fn set_reachability(&mut self, on: bool) {
        self.servers.reach_on = on;
        if !on {
            self.servers.reach.clear();
        }
        self.core.send(Command::SetSetting {
            key: REACH_SETTING.into(),
            value: on.into(),
        });
        self.sync_reach();
    }

    fn reach_dot(&self, id: &ProfileId, p: &Palette) -> (Hsla, String) {
        if !self.servers.reach_on {
            return (gpui_kit::transparent_black(), String::new());
        }
        match self.servers.reach.get(id) {
            Some(ReachStatus::Up(ms)) => (p.dev, format!("Reachable · {ms} ms")),
            Some(ReachStatus::Down(e)) => (p.prod, format!("Not reachable · {e}")),
            Some(ReachStatus::Indirect) => {
                (p.fg3, "Through a jump Host or tunnel · not checked".into())
            }
            None => (p.bd2, "Checking…".into()),
        }
    }

    fn server_action(
        &mut self,
        id: &ProfileId,
        action: &str,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) {
        let profile = self.profiles.all.iter().find(|p| p.id() == id).cloned();
        match (action, profile) {
            ("ssh", Some(Profile::Host(_))) => self.open_terminal(Some(id.clone()), cx),
            ("sftp", Some(Profile::Host(_))) => self.open_files_for(Some(id.clone()), cx),
            ("fleet", Some(Profile::Host(_))) => self.open_fleet(
                crate::fleet_tab::Mode::Dashboard,
                crate::fleet_tab::model::Pick::Hosts(vec![id.clone()]),
                window,
                cx,
            ),
            ("desktop", Some(Profile::Desktop(_))) => self.launch_desktop(id, cx),
            ("edit", Some(Profile::Desktop(d))) => {
                self.open_desktop_editor(d.protocol, Some(d), window, cx)
            }
            ("edit", Some(p @ Profile::Host(_))) => {
                self.open_conn_editor(crate::conn_editor::ConnKind::Ssh, Some(p), window, cx)
            }
            _ => {}
        }
        cx.notify();
    }

    fn server_buttons(
        &self,
        it: &ServerItem,
        key: usize,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> gpui_kit::Div {
        let btn =
            |label: &'static str, action: &'static str, primary: bool, cx: &mut Context<Self>| {
                let id = it.id.clone();
                ui::button(
                    SharedString::from(format!("srv-{action}-{key}")),
                    label,
                    if primary {
                        Kind::Secondary
                    } else {
                        Kind::Ghost
                    },
                    p,
                )
                .h(rpx(22.))
                .on_click(cx.listener(move |this, _, w, cx| {
                    cx.stop_propagation();
                    this.server_action(&id, action, w, cx);
                }))
            };
        let mut row = div().flex().flex_none().items_center().gap(rpx(4.));
        match it.kind {
            ServerKind::Host => {
                row = row
                    .child(btn("SSH", "ssh", true, cx))
                    .child(btn("SFTP", "sftp", false, cx))
                    .child(btn("Fleet", "fleet", false, cx));
            }
            ServerKind::Desktop(DesktopProtocol::Rdp) => {
                row = row.child(btn("RDP", "desktop", true, cx))
            }
            ServerKind::Desktop(DesktopProtocol::Vnc) => {
                row = row.child(btn("VNC", "desktop", true, cx))
            }
        }
        row.child(btn("Edit", "edit", false, cx))
    }

    fn server_click(
        &mut self,
        id: &ProfileId,
        ev: &gpui_kit::ClickEvent,
        window: &mut gpui_kit::Window,
        cx: &mut Context<Self>,
    ) {
        if ev.modifiers().secondary() {
            let sel = &mut self.servers.view.selection;
            match sel.iter().position(|x| x == id) {
                Some(i) => {
                    sel.remove(i);
                }
                None => sel.push(id.clone()),
            }
        } else if ev.click_count() >= 2 {
            match self.profiles.all.iter().find(|p| p.id() == id) {
                Some(Profile::Host(_)) => self.server_action(id, "ssh", window, cx),
                Some(Profile::Desktop(_)) => self.server_action(id, "desktop", window, cx),
                _ => {}
            }
        } else {
            self.servers.view.selection = vec![id.clone()];
        }
        cx.notify();
    }

    fn server_menu(&mut self, id: &ProfileId, ev: &MouseDownEvent, cx: &mut Context<Self>) {
        let sel = self.servers.view.selection.clone();
        let target = if sel.len() > 1 && sel.contains(id) {
            if sel.iter().all(|s| self.profiles.host(s).is_some()) {
                CtxTarget::Hosts(sel)
            } else {
                CtxTarget::Items(sel)
            }
        } else {
            CtxTarget::Profile(id.clone())
        };
        self.ctx = Some(CtxMenu::new(ev.position, target));
        cx.notify();
    }

    fn render_server_row(
        &self,
        it: &ServerItem,
        i: usize,
        now: i64,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (dot, tip) = self.reach_dot(&it.id, p);
        let picked = self.servers.view.selection.contains(&it.id);
        let (id1, id2) = (it.id.clone(), it.id.clone());
        let badge = match it.kind {
            ServerKind::Host => "SSH",
            ServerKind::Desktop(pr) => pr.badge(),
        };
        div()
            .id(("srv-row", i))
            .w_full()
            .h(rpx(34.))
            .flex()
            .items_center()
            .gap(rpx(10.))
            .px(rpx(12.))
            .border_b_1()
            .border_color(p.bd)
            .text_size(ts::UI)
            .when(picked, |d| d.bg(p.sel))
            .hover(|s| s.bg(p.hover))
            .on_click(cx.listener(move |this, ev: &gpui_kit::ClickEvent, w, cx| {
                this.server_click(&id1, ev, w, cx)
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| this.server_menu(&id2, ev, cx)),
            )
            .child(
                div()
                    .id(("srv-dot", i))
                    .flex_none()
                    .tooltip(move |w, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(w, cx)
                    })
                    .child(ui::dot(dot, 8.)),
            )
            .child(ui::monogram(badge, 32., p))
            .child(
                div()
                    .w(rpx(170.))
                    .flex_none()
                    .truncate()
                    .font_weight(FontWeight::MEDIUM)
                    .child(it.name.clone()),
            )
            .child(
                div()
                    .w(rpx(190.))
                    .flex_none()
                    .truncate()
                    .font_family(MONO)
                    .text_size(ts::BODY)
                    .text_color(p.fg2)
                    .child(it.address.clone()),
            )
            .child(
                div()
                    .w(rpx(150.))
                    .flex_none()
                    .truncate()
                    .text_color(p.fg2)
                    .child(it.os.clone().unwrap_or_else(|| "—".into())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .child(
                        it.tags
                            .iter()
                            .map(|t| format!("#{t}"))
                            .collect::<Vec<_>>()
                            .join(" "),
                    ),
            )
            .child(
                div()
                    .w(rpx(96.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(rpx(5.))
                    .text_size(ts::SMALL)
                    .child(ui::dot(p.env(it.env), 6.))
                    .child(it.env.name()),
            )
            .child(
                div()
                    .w(rpx(84.))
                    .flex_none()
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .child(ago(it.last, now)),
            )
            .child(self.server_buttons(it, i, p, cx))
            .into_any_element()
    }

    fn render_server_card(
        &self,
        it: &ServerItem,
        key: usize,
        now: i64,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (dot, _) = self.reach_dot(&it.id, p);
        let picked = self.servers.view.selection.contains(&it.id);
        let (id1, id2) = (it.id.clone(), it.id.clone());
        let badge = match it.kind {
            ServerKind::Host => "SSH",
            ServerKind::Desktop(pr) => pr.badge(),
        };
        div()
            .id(("srv-card", key))
            .flex_1()
            .min_w_0()
            .h(rpx(118.))
            .flex()
            .flex_col()
            .gap(rpx(6.))
            .p(rpx(12.))
            .border_1()
            .border_color(if picked { p.acc } else { p.bd })
            .rounded(px(8.))
            .bg(p.surface)
            .hover(|s| s.border_color(p.fg3))
            .on_click(cx.listener(move |this, ev: &gpui_kit::ClickEvent, w, cx| {
                this.server_click(&id1, ev, w, cx)
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| this.server_menu(&id2, ev, cx)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(rpx(8.))
                    .child(ui::dot(dot, 8.))
                    .child(ui::monogram(badge, 32., p))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(it.name.clone()),
                    )
                    .child(ui::dot(p.env(it.env), 6.)),
            )
            .child(
                div()
                    .truncate()
                    .font_family(MONO)
                    .text_size(ts::BODY)
                    .text_color(p.fg2)
                    .child(it.address.clone()),
            )
            .child(
                div()
                    .truncate()
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .child(format!(
                        "{} · {}",
                        it.os.clone().unwrap_or_else(|| "OS unknown".into()),
                        ago(it.last, now)
                    )),
            )
            .child(div().flex_1())
            .child(self.server_buttons(it, key, p, cx))
            .into_any_element()
    }

    /// The Servers view.
    pub(crate) fn render_servers(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let p = *p;
        let search = self.servers.view.search.clone();
        let query = search
            .as_ref()
            .map(|s| s.read(cx).value().to_string())
            .unwrap_or_default();
        let items = server_items(&self.servers.scoped, &self.servers.facts);
        let rows = server_rows(&items, &query, self.servers.view.group);
        let recents = if query.trim().is_empty() {
            recent_items(&items, RECENTS)
        } else {
            Vec::new()
        };
        let now = switchyard_core::store::now_ms();
        let cards = self.servers.view.cards;
        let group = self.servers.view.group;
        let reach_on = self.servers.reach_on;
        let n_items = items.len();
        let seg = |label: &'static str,
                   on: bool,
                   f: fn(&mut Workspace),
                   cx: &mut Context<Self>|
         -> (SharedString, bool, ui::OnClick) {
            (
                label.into(),
                on,
                Box::new(cx.listener(move |this, _, _, cx| {
                    f(this);
                    cx.notify();
                })),
            )
        };
        let toolbar = div()
            .flex_none()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(rpx(10.))
            .px(rpx(16.))
            .py(rpx(10.))
            .border_b_1()
            .border_color(p.bd)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(ts::TITLE)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Servers"),
                    )
                    .child(div().text_size(ts::SMALL).text_color(p.fg3).child(format!(
                        "{} · {n_items} server{}",
                        self.workspace_name,
                        if n_items == 1 { "" } else { "s" }
                    ))),
            )
            .child(
                div()
                    .w(rpx(260.))
                    .h(rpx(28.))
                    .flex()
                    .items_center()
                    .px(rpx(8.))
                    .border_1()
                    .border_color(p.bd2)
                    .rounded(px(6.))
                    .bg(p.bg)
                    .children(
                        search
                            .as_ref()
                            .map(|s| Input::new(s).appearance(false).text_size(ts::BODY)),
                    ),
            )
            .child(ui::segmented(
                "srv-mode",
                vec![
                    seg("List", !cards, |w| w.servers.view.cards = false, cx),
                    seg("Cards", cards, |w| w.servers.view.cards = true, cx),
                ],
                22.,
                &p,
            ))
            .child(ui::segmented(
                "srv-group",
                vec![
                    seg(
                        "All",
                        group == ServerGroup::None,
                        |w| w.servers.view.group = ServerGroup::None,
                        cx,
                    ),
                    seg(
                        "Folder",
                        group == ServerGroup::Folder,
                        |w| w.servers.view.group = ServerGroup::Folder,
                        cx,
                    ),
                    seg(
                        "Tag",
                        group == ServerGroup::Tag,
                        |w| w.servers.view.group = ServerGroup::Tag,
                        cx,
                    ),
                    seg(
                        "Environment",
                        group == ServerGroup::Environment,
                        |w| w.servers.view.group = ServerGroup::Environment,
                        cx,
                    ),
                ],
                22.,
                &p,
            ))
            .child(
                ui::checkbox("srv-reach", reach_on, "Check reachability", &p).on_click(
                    cx.listener(move |this, _, _, cx| {
                        this.set_reachability(!reach_on);
                        cx.notify();
                    }),
                ),
            )
            .child(div().flex_1())
            .child(
                ui::button("srv-new-host", "New Host…", Kind::Ghost, &p).on_click(cx.listener(
                    |this, _, w, cx| {
                        this.open_conn_editor(crate::conn_editor::ConnKind::Ssh, None, w, cx)
                    },
                )),
            )
            .child(
                ui::button("srv-new-rdp", "New RDP…", Kind::Ghost, &p).on_click(cx.listener(
                    |this, _, w, cx| this.open_desktop_editor(DesktopProtocol::Rdp, None, w, cx),
                )),
            )
            .child(
                ui::button("srv-new-vnc", "New VNC…", Kind::Ghost, &p).on_click(cx.listener(
                    |this, _, w, cx| this.open_desktop_editor(DesktopProtocol::Vnc, None, w, cx),
                )),
            );
        let recent_strip = (!recents.is_empty()).then(|| {
            div()
                .flex_none()
                .flex()
                .flex_col()
                .gap(rpx(6.))
                .px(rpx(16.))
                .pt(rpx(10.))
                .child(ui::caption("RECENT", &p))
                .child(
                    div()
                        .flex()
                        .gap(rpx(8.))
                        .children(recents.iter().enumerate().map(|(i, it)| {
                            let id = it.id.clone();
                            let (dot, _) = self.reach_dot(&it.id, &p);
                            div()
                                .id(("srv-recent", i))
                                .flex()
                                .items_center()
                                .gap(rpx(6.))
                                .px(rpx(10.))
                                .h(rpx(28.))
                                .border_1()
                                .border_color(p.bd)
                                .rounded(px(6.))
                                .text_size(ts::BODY)
                                .cursor_pointer()
                                .hover(|s| s.bg(p.hover))
                                .on_click(cx.listener(move |this, _, w, cx| {
                                    let action =
                                        match this.profiles.all.iter().find(|p| p.id() == &id) {
                                            Some(Profile::Desktop(_)) => "desktop",
                                            _ => "ssh",
                                        };
                                    this.server_action(&id, action, w, cx)
                                }))
                                .child(ui::dot(dot, 6.))
                                .child(it.name.clone())
                                .child(
                                    div()
                                        .text_size(ts::SMALL)
                                        .text_color(p.fg3)
                                        .child(ago(it.last, now)),
                                )
                        })),
                )
        });
        let body: AnyElement = if rows.is_empty() {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(rpx(8.))
                .text_color(p.fg2)
                .child(if n_items == 0 {
                    "No servers in this workspace yet"
                } else {
                    "Nothing matches the search"
                })
                .child(div().text_size(ts::BODY).text_color(p.fg3).child(
                    "Add a Host or a remote desktop, or add existing ones to this workspace from the Explorer (right click › Add to workspace).",
                ))
                .into_any_element()
        } else if cards {
            // Rows of cards; group headings take a row of their own.
            let mut lines: Vec<Vec<ServerRow>> = Vec::new();
            for r in rows {
                match r {
                    ServerRow::Group(..) => lines.push(vec![r]),
                    ServerRow::Item(_) => match lines.last_mut() {
                        Some(l)
                            if l.len() < CARDS_PER_ROW
                                && matches!(l.first(), Some(ServerRow::Item(_))) =>
                        {
                            l.push(r)
                        }
                        _ => lines.push(vec![r]),
                    },
                }
            }
            let n = lines.len();
            uniform_list(
                "srv-cards",
                n,
                cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                    range
                        .map(|i| {
                            let line = &lines[i];
                            let mut d = div()
                                .w_full()
                                .h(rpx(130.))
                                .flex()
                                .gap(rpx(10.))
                                .px(rpx(16.))
                                .py(rpx(6.));
                            for (j, r) in line.iter().enumerate() {
                                match r {
                                    ServerRow::Group(label, count) => {
                                        d = d.items_end().child(ui::caption(
                                            format!("{} · {count}", label.to_uppercase()),
                                            &p,
                                        ));
                                    }
                                    ServerRow::Item(it) => {
                                        d = d.child(this.render_server_card(
                                            it,
                                            i * CARDS_PER_ROW + j,
                                            now,
                                            &p,
                                            cx,
                                        ));
                                    }
                                }
                            }
                            for _ in line.len()..CARDS_PER_ROW {
                                if matches!(line.first(), Some(ServerRow::Item(_))) {
                                    d = d.child(div().flex_1());
                                }
                            }
                            d.into_any_element()
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .flex_1()
            .into_any_element()
        } else {
            let n = rows.len();
            uniform_list(
                "srv-rows",
                n,
                cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                    range
                        .map(|i| match &rows[i] {
                            ServerRow::Group(label, count) => div()
                                .w_full()
                                .h(rpx(34.))
                                .flex()
                                .items_end()
                                .px(rpx(16.))
                                .pb(rpx(6.))
                                .child(ui::caption(
                                    format!("{} · {count}", label.to_uppercase()),
                                    &p,
                                ))
                                .into_any_element(),
                            ServerRow::Item(it) => this.render_server_row(it, i, now, &p, cx),
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .flex_1()
            .into_any_element()
        };
        let picked = self.servers.view.selection.len();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.surface)
            .child(toolbar)
            .children(recent_strip)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .pt(rpx(6.))
                    .child(body),
            )
            .when(picked > 1, |d| {
                d.child(
                    div()
                        .flex_none()
                        .px(rpx(16.))
                        .py(rpx(6.))
                        .border_t_1()
                        .border_color(p.bd)
                        .text_size(ts::SMALL)
                        .text_color(p.fg3)
                        .child(format!(
                            "{picked} selected · right click to add them to a workspace"
                        )),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::store::{DesktopServer, Host};

    fn host(
        name: &str,
        addr: &str,
        folder: Option<&str>,
        tags: &[&str],
        env: EnvironmentLabel,
    ) -> Profile {
        let mut h = Host::new(name, addr, "root");
        h.folder = folder.map(str::to_owned);
        h.tags = tags.iter().map(|t| (*t).to_owned()).collect();
        h.environment = env;
        Profile::Host(h)
    }

    fn sample() -> (Profiles, HashMap<ProfileId, ServerFacts>) {
        let web = host(
            "web-1",
            "10.0.0.5",
            Some("Prod"),
            &["web"],
            EnvironmentLabel::Production,
        );
        let db = host(
            "db-1",
            "10.0.0.9",
            Some("Prod"),
            &["db"],
            EnvironmentLabel::Production,
        );
        let lab = host(
            "lab",
            "192.168.1.20",
            None,
            &[],
            EnvironmentLabel::Development,
        );
        let mut dc = DesktopServer::new("dc01", DesktopProtocol::Rdp);
        dc.address = "dc01.corp".into();
        dc.via_host = Some(lab.id().clone());
        dc.environment = EnvironmentLabel::Staging;
        dc.tags = vec!["windows".into()];
        let dc = Profile::Desktop(dc);
        let mut facts = HashMap::new();
        facts.insert(
            web.id().clone(),
            ServerFacts {
                id: web.id().clone(),
                os: Some("Ubuntu 24.04".into()),
                last_connected: Some(5_000),
            },
        );
        facts.insert(
            dc.id().clone(),
            ServerFacts {
                id: dc.id().clone(),
                os: None,
                last_connected: Some(9_000),
            },
        );
        let d = switchyard_core::store::DbConnection::new(
            "not a server",
            switchyard_core::db::Engine::Postgres,
        );
        (
            Profiles {
                all: vec![web, db, lab, dc, Profile::Db(d)],
            },
            facts,
        )
    }

    fn names(rows: &[ServerRow]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                ServerRow::Group(l, n) => format!("[{l} {n}]"),
                ServerRow::Item(it) => it.name.clone(),
            })
            .collect()
    }

    #[test]
    fn servers_are_hosts_and_desktops_with_their_facts() {
        let (profiles, facts) = sample();
        let items = server_items(&profiles, &facts);
        assert_eq!(items.len(), 4);
        assert_eq!(items[0].os.as_deref(), Some("Ubuntu 24.04"));
        assert_eq!(items[3].address, "dc01.corp via lab");
        assert_eq!(items[3].kind, ServerKind::Desktop(DesktopProtocol::Rdp));
    }

    #[test]
    fn search_by_name_ip_and_tag() {
        let (profiles, facts) = sample();
        let items = server_items(&profiles, &facts);
        assert_eq!(
            names(&server_rows(&items, "db", ServerGroup::None)),
            ["db-1", "dc01"]
        );
        assert_eq!(
            names(&server_rows(&items, "192.168", ServerGroup::None)),
            ["lab"]
        );
        assert_eq!(
            names(&server_rows(&items, "tag:web", ServerGroup::None)),
            ["web-1"]
        );
        assert_eq!(
            names(&server_rows(&items, "ubuntu", ServerGroup::None)),
            ["web-1"]
        );
        assert!(server_rows(&items, "zzz", ServerGroup::None).is_empty());
    }

    #[test]
    fn grouping_by_folder_tag_and_environment() {
        let (profiles, facts) = sample();
        let items = server_items(&profiles, &facts);
        assert_eq!(
            names(&server_rows(&items, "", ServerGroup::Folder)),
            ["[Prod 2]", "web-1", "db-1", "[No folder 2]", "lab", "dc01"]
        );
        assert_eq!(
            names(&server_rows(&items, "", ServerGroup::Tag)),
            [
                "[# db 1]",
                "db-1",
                "[# web 1]",
                "web-1",
                "[# windows 1]",
                "dc01",
                "[Untagged 1]",
                "lab"
            ]
        );
        assert_eq!(
            names(&server_rows(&items, "", ServerGroup::Environment)),
            [
                "[Production 2]",
                "web-1",
                "db-1",
                "[Staging 1]",
                "dc01",
                "[Development 1]",
                "lab"
            ]
        );
    }

    #[test]
    fn recents_newest_first_and_relative_times() {
        let (profiles, facts) = sample();
        let items = server_items(&profiles, &facts);
        let r: Vec<String> = recent_items(&items, 5)
            .into_iter()
            .map(|i| i.name)
            .collect();
        assert_eq!(r, ["dc01", "web-1"]);
        assert_eq!(ago(None, 0), "never");
        assert_eq!(ago(Some(0), 30_000), "just now");
        assert_eq!(ago(Some(0), 5 * 60_000), "5 min ago");
        assert_eq!(ago(Some(0), 3 * 3_600_000), "3 h ago");
        assert_eq!(ago(Some(0), 2 * 86_400_000), "2 d ago");
    }
}
