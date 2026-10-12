//! Log viewer: tail a file or the systemd journal on an SSH Host, follow a CloudWatch Logs
//! group, or re-run an Azure Log Analytics (KQL) query. Lines stream in batches into a
//! capped, virtualized list with filter, minimum level, highlighted terms, pause, copy and
//! export. Stop (or closing the tab) cancels the stream.

use std::collections::VecDeque;
use std::ops::Range;

use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, FontWeight, HighlightStyle, Hsla,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollStrategy, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledText, Subscription,
    UniformListScrollHandle, Window, div, uniform_list,
};
use switchyard_core::cloud::inventory::azure::LaWorkspace;
use switchyard_core::cloud::logs::LogGroup;
use switchyard_core::infra::logline::{Level, LogLine, iso};
use switchyard_core::infra::{InfraOp, InfraUpdate, LogSource};
use switchyard_core::store::{CloudProvider, Profile, ProfileId};
use switchyard_core::{Command, RequestId, RuntimeHandle};

use super::{field, input_box, pick, plural, status_line};
use crate::app_state::{Profiles, next_id};
use crate::appearance::{rpx, ts};
use crate::theme::{MONO, Palette, SANS, palette};
use crate::ui::{self, Kind};

/// Lines kept; older ones are dropped in chunks.
const MAX_LINES: usize = 100_000;
const ROW_H: f32 = 20.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SrcKind {
    File,
    Journal,
    CloudWatch,
    LogAnalytics,
}

impl SrcKind {
    fn label(self) -> &'static str {
        match self {
            SrcKind::File => "File",
            SrcKind::Journal => "journald",
            SrcKind::CloudWatch => "CloudWatch",
            SrcKind::LogAnalytics => "Log Analytics",
        }
    }
}

/// Why a request was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Purpose {
    Tail,
    Groups,
    Workspaces,
}

/// Sorted, merged byte ranges of `text` where any of `terms` occurs (ASCII case-insensitive).
pub(crate) fn highlight_ranges(text: &str, terms: &[String]) -> Vec<Range<usize>> {
    let hay = text.to_ascii_lowercase();
    let mut out: Vec<Range<usize>> = Vec::new();
    for t in terms {
        let needle = t.to_ascii_lowercase();
        if needle.is_empty() {
            continue;
        }
        let mut from = 0;
        while let Some(i) = hay[from..].find(&needle) {
            let s = from + i;
            out.push(s..s + needle.len());
            from = s + needle.len();
        }
    }
    out.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for r in out {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    // Only whole characters (a match can't split one, but stay safe).
    merged.retain(|r| text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
    merged
}

/// One display line for a message.
fn one_line(s: &str) -> String {
    s.trim_end().replace('\n', " ⏎ ")
}

/// The log viewer.
pub struct LogsTab {
    core: RuntimeHandle,
    profiles: Profiles,
    kind: SrcKind,
    host: Option<ProfileId>,
    conn: Option<ProfileId>,
    path: Entity<InputState>,
    unit: Entity<InputState>,
    region: Entity<InputState>,
    group: Entity<InputState>,
    pattern: Entity<InputState>,
    query: Entity<InputState>,
    since: Entity<InputState>,
    groups: Vec<LogGroup>,
    workspaces: Vec<LaWorkspace>,
    workspace: Option<String>,
    filter: Entity<InputState>,
    filter_text: String,
    highlight: Entity<InputState>,
    min_level: Option<Level>,
    pending: Vec<(RequestId, Purpose)>,
    stream: Option<RequestId>,
    title: String,
    lines: VecDeque<LogLine>,
    held: Vec<LogLine>,
    paused: bool,
    shown: Vec<usize>,
    dropped: usize,
    follow: bool,
    scroll: UniformListScrollHandle,
    status: Option<(bool, String)>,
    _subs: Vec<Subscription>,
}

impl LogsTab {
    /// A new, idle viewer.
    pub fn new(
        core: RuntimeHandle,
        profiles: Profiles,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = |ph: &str, window: &mut Window, cx: &mut Context<Self>| {
            let ph = ph.to_owned();
            cx.new(|cx| InputState::new(window, cx).placeholder(ph))
        };
        let path = input("/var/log/syslog", window, cx);
        let unit = input("Unit (all when empty)", window, cx);
        let region = input("Region (connection's when empty)", window, cx);
        let group = input("/aws/lambda/…", window, cx);
        let pattern = input("Filter pattern (CloudWatch syntax)", window, cx);
        let query = input(
            "AppTraces | project TimeGenerated, SeverityLevel, Message",
            window,
            cx,
        );
        let since = input("15", window, cx);
        let filter = input("Filter lines", window, cx);
        let highlight = input("Highlight terms (comma separated)", window, cx);
        let subs = vec![
            cx.subscribe_in(&filter, window, |t, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    t.refilter(cx);
                }
            }),
            cx.subscribe_in(&highlight, window, |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&path, window, |t, _, ev: &InputEvent, _, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    t.start(cx);
                }
            }),
        ];
        Self {
            core,
            profiles,
            kind: SrcKind::File,
            host: None,
            conn: None,
            path,
            unit,
            region,
            group,
            pattern,
            query,
            since,
            groups: Vec::new(),
            workspaces: Vec::new(),
            workspace: None,
            filter,
            filter_text: String::new(),
            highlight,
            min_level: None,
            pending: Vec::new(),
            stream: None,
            title: String::new(),
            lines: VecDeque::new(),
            held: Vec::new(),
            paused: false,
            shown: Vec::new(),
            dropped: 0,
            follow: true,
            scroll: UniformListScrollHandle::default(),
            status: None,
            _subs: subs,
        }
    }

    /// Tab title.
    pub fn title(&self) -> String {
        if self.title.is_empty() {
            "Logs".into()
        } else {
            format!("Logs · {}", self.title)
        }
    }

    /// Whether `request` is this tab's.
    pub fn owns(&self, request: RequestId) -> bool {
        self.pending.iter().any(|(r, _)| *r == request)
    }

    /// New profile list.
    pub fn set_profiles(&mut self, profiles: Profiles, cx: &mut Context<Self>) {
        self.profiles = profiles;
        cx.notify();
    }

    fn send(&mut self, op: InfraOp, purpose: Purpose) -> RequestId {
        let request = next_id();
        self.pending.push((request, purpose));
        self.core.send(Command::Infra { request, op });
        request
    }

    fn value(e: &Entity<InputState>, cx: &Context<Self>) -> String {
        e.read(cx).value().trim().to_owned()
    }

    fn hosts(&self) -> Vec<(String, bool, Option<ProfileId>)> {
        self.profiles
            .all
            .iter()
            .filter_map(|p| match p {
                Profile::Host(h) => Some((
                    h.name.clone(),
                    self.host.as_ref() == Some(&h.id),
                    Some(h.id.clone()),
                )),
                _ => None,
            })
            .collect()
    }

    fn conns(&self, provider: CloudProvider) -> Vec<(String, bool, Option<ProfileId>)> {
        self.profiles
            .all
            .iter()
            .filter_map(|p| match p {
                Profile::Cloud(c) if c.service.provider() == provider => Some((
                    c.name.clone(),
                    self.conn.as_ref() == Some(&c.id),
                    Some(c.id.clone()),
                )),
                _ => None,
            })
            .collect()
    }

    fn set_kind(&mut self, k: SrcKind, cx: &mut Context<Self>) {
        if self.kind != k {
            self.kind = k;
            self.conn = None;
        }
        cx.notify();
    }

    fn set_host(&mut self, h: Option<ProfileId>, cx: &mut Context<Self>) {
        self.host = h;
        cx.notify();
    }

    fn set_conn(&mut self, c: Option<ProfileId>, cx: &mut Context<Self>) {
        self.conn = c;
        self.groups.clear();
        self.workspaces.clear();
        self.workspace = None;
        cx.notify();
    }

    fn set_workspace(&mut self, w: String, cx: &mut Context<Self>) {
        self.workspace = Some(w);
        cx.notify();
    }

    fn set_group(&mut self, g: String, window: &mut Window, cx: &mut Context<Self>) {
        self.group.update(cx, |i, cx| i.set_value(g, window, cx));
        cx.notify();
    }

    fn set_min_level(&mut self, l: Option<Level>, cx: &mut Context<Self>) {
        self.min_level = l;
        self.refilter(cx);
    }

    fn load_groups(&mut self, cx: &mut Context<Self>) {
        let Some(conn) = self.conn.clone() else {
            self.status = Some((false, "Choose an AWS connection".into()));
            cx.notify();
            return;
        };
        let region = Self::value(&self.region, cx);
        let prefix = Self::value(&self.group, cx);
        self.send(
            InfraOp::LogGroups {
                conn,
                region,
                prefix,
            },
            Purpose::Groups,
        );
        self.status = Some((true, "Listing log groups…".into()));
        cx.notify();
    }

    fn load_workspaces(&mut self, cx: &mut Context<Self>) {
        let conn = self.conn.clone();
        self.send(InfraOp::Workspaces { conn }, Purpose::Workspaces);
        self.status = Some((true, "Listing Log Analytics workspaces…".into()));
        cx.notify();
    }

    fn source(&self, cx: &Context<Self>) -> Result<(LogSource, String), String> {
        let since = Self::value(&self.since, cx)
            .parse::<u32>()
            .unwrap_or(15)
            .max(1);
        Ok(match self.kind {
            SrcKind::File => {
                let host = self.host.clone().ok_or("Choose a Host")?;
                let path = Self::value(&self.path, cx);
                if !path.starts_with('/') {
                    return Err("Enter an absolute path".into());
                }
                let short = path.rsplit('/').next().unwrap_or(&path).to_owned();
                (
                    LogSource::HostFile {
                        host,
                        path,
                        lines: 200,
                    },
                    short,
                )
            }
            SrcKind::Journal => {
                let host = self.host.clone().ok_or("Choose a Host")?;
                let unit = Some(Self::value(&self.unit, cx)).filter(|u| !u.is_empty());
                let title = unit.clone().unwrap_or_else(|| "journal".into());
                (
                    LogSource::Journal {
                        host,
                        unit,
                        lines: 200,
                    },
                    title,
                )
            }
            SrcKind::CloudWatch => {
                let conn = self.conn.clone().ok_or("Choose an AWS connection")?;
                let group = Self::value(&self.group, cx);
                if group.is_empty() {
                    return Err("Enter or choose a log group".into());
                }
                let short = group.rsplit('/').next().unwrap_or(&group).to_owned();
                (
                    LogSource::CloudWatch {
                        conn,
                        region: Self::value(&self.region, cx),
                        group,
                        pattern: Self::value(&self.pattern, cx),
                        since_mins: since,
                        follow: true,
                    },
                    short,
                )
            }
            SrcKind::LogAnalytics => {
                let workspace = self.workspace.clone().ok_or("Choose a workspace")?;
                let query = Self::value(&self.query, cx);
                if query.is_empty() {
                    return Err("Enter a KQL query".into());
                }
                let name = self
                    .workspaces
                    .iter()
                    .find(|w| w.customer_id == workspace)
                    .map_or_else(|| "KQL".to_owned(), |w| w.name.clone());
                (
                    LogSource::LogAnalytics {
                        conn: self.conn.clone(),
                        workspace,
                        query,
                        since_mins: since,
                        follow: true,
                    },
                    name,
                )
            }
        })
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        let (src, title) = match self.source(cx) {
            Ok(s) => s,
            Err(e) => {
                self.status = Some((false, e));
                cx.notify();
                return;
            }
        };
        self.stop(cx);
        self.lines.clear();
        self.held.clear();
        self.shown.clear();
        self.dropped = 0;
        self.paused = false;
        self.follow = true;
        self.title = title;
        self.status = Some((true, "Starting…".into()));
        self.stream = Some(self.send(InfraOp::Tail(src), Purpose::Tail));
        cx.notify();
    }

    /// Stop the stream, if any.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(r) = self.stream.take() {
            self.core.send(Command::InfraCancel { request: r });
            self.pending.retain(|(p, _)| *p != r);
            self.status = Some((true, "Stopped".into()));
            cx.notify();
        }
    }

    fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        self.paused = !self.paused;
        if !self.paused {
            let held = std::mem::take(&mut self.held);
            self.append(held);
        }
        cx.notify();
    }

    fn append(&mut self, batch: Vec<LogLine>) {
        let filter = self.filter_text.clone();
        let base = self.dropped;
        for l in batch {
            if l.matches(&filter, self.min_level) {
                self.shown.push(base + self.lines.len());
            }
            self.lines.push_back(l);
        }
        if self.lines.len() > MAX_LINES {
            let n = self.lines.len() - MAX_LINES + MAX_LINES / 10;
            self.lines.drain(..n);
            self.dropped += n;
            let first = self.dropped;
            self.shown.retain(|i| *i >= first);
        }
        if self.follow && !self.shown.is_empty() {
            self.scroll
                .scroll_to_item(self.shown.len() - 1, ScrollStrategy::Bottom);
        }
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        self.filter_text = Self::value(&self.filter, cx);
        let base = self.dropped;
        self.shown = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.matches(&self.filter_text, self.min_level))
            .map(|(i, _)| base + i)
            .collect();
        cx.notify();
    }

    fn visible_text(&self) -> String {
        let mut out = String::new();
        for i in &self.shown {
            if let Some(l) = self.lines.get(i - self.dropped) {
                out.push_str(&l.to_text());
                out.push('\n');
            }
        }
        out
    }

    fn copy_all(&mut self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(self.visible_text()));
        self.status = Some((true, format!("Copied {}", plural(self.shown.len(), "line"))));
        cx.notify();
    }

    fn export(&mut self, cx: &mut Context<Self>) {
        let text = self.visible_text();
        let dir = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default();
        let name = format!(
            "{}.log",
            if self.title.is_empty() {
                "logs"
            } else {
                &self.title
            }
            .replace(['/', '\\', ' '], "_")
        );
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        let core = self.core.clone();
        cx.spawn(async move |_, _| {
            if let Ok(Ok(Some(path))) = rx.await {
                core.send(Command::WriteFile {
                    path,
                    contents: text,
                });
            }
        })
        .detach();
    }

    /// An update for one of this tab's requests.
    pub fn on_update(
        &mut self,
        request: RequestId,
        update: InfraUpdate,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(purpose) = self
            .pending
            .iter()
            .find(|(r, _)| *r == request)
            .map(|(_, p)| *p)
        else {
            return;
        };
        match (purpose, update) {
            (Purpose::Tail, InfraUpdate::LogLines(batch)) => {
                if self.paused {
                    self.held.extend(batch);
                } else {
                    self.append(batch);
                }
            }
            (Purpose::Tail, InfraUpdate::LogStatus(s)) => self.status = Some((true, s)),
            (Purpose::Tail, InfraUpdate::LogEnded(e)) => {
                self.pending.retain(|(r, _)| *r != request);
                if self.stream == Some(request) {
                    self.stream = None;
                }
                self.status = Some(match e {
                    Some(e) => (false, e),
                    None => (true, "Ended".into()),
                });
            }
            (Purpose::Groups, InfraUpdate::LogGroups(r)) => {
                self.pending.retain(|(r, _)| *r != request);
                match r {
                    Ok(g) => {
                        self.status = Some((true, plural(g.len(), "log group")));
                        self.groups = g;
                    }
                    Err(e) => self.status = Some((false, e)),
                }
            }
            (Purpose::Workspaces, InfraUpdate::Workspaces(r)) => {
                self.pending.retain(|(r, _)| *r != request);
                match r {
                    Ok(w) => {
                        self.status = Some((true, plural(w.len(), "workspace")));
                        if self.workspace.is_none() {
                            self.workspace = w.first().map(|w| w.customer_id.clone());
                        }
                        self.workspaces = w;
                    }
                    Err(e) => self.status = Some((false, e)),
                }
            }
            (_, InfraUpdate::Done(Err(e))) => {
                self.pending.retain(|(r, _)| *r != request);
                self.status = Some((false, e));
            }
            _ => {}
        }
        cx.notify();
    }

    // ---------------------------------------------------------------- render

    fn render_source(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let kinds: Vec<(SharedString, bool, ui::OnClick)> = [
            SrcKind::File,
            SrcKind::Journal,
            SrcKind::CloudWatch,
            SrcKind::LogAnalytics,
        ]
        .into_iter()
        .map(|k| {
            let this = this.clone();
            let cb: ui::OnClick = Box::new(move |_, _, cx| {
                let _ = this.update(cx, |t, cx| t.set_kind(k, cx));
            });
            (SharedString::from(k.label()), self.kind == k, cb)
        })
        .collect();
        let boxed = |e: &Entity<InputState>, w: f32| {
            input_box(p)
                .w(rpx(w))
                .child(Input::new(e).appearance(false).text_size(ts::BODY))
        };
        let mut row = div()
            .flex()
            .flex_wrap()
            .items_end()
            .gap(rpx(8.))
            .child(field("Source", ui::segmented("log-kind", kinds, 22., p), p));
        match self.kind {
            SrcKind::File | SrcKind::Journal => {
                let hosts = self.hosts();
                let current = hosts
                    .iter()
                    .find(|h| h.1)
                    .map_or_else(|| "Choose a Host".to_owned(), |h| h.0.clone());
                row = row.child(field(
                    "Host",
                    pick("log-host", current, hosts, this.clone(), Self::set_host),
                    p,
                ));
                row = if self.kind == SrcKind::File {
                    row.child(field("File", boxed(&self.path, 300.), p))
                } else {
                    row.child(field("Unit", boxed(&self.unit, 200.), p))
                };
            }
            SrcKind::CloudWatch => {
                let conns = self.conns(CloudProvider::Aws);
                let current = conns
                    .iter()
                    .find(|h| h.1)
                    .map_or_else(|| "Choose an AWS connection".to_owned(), |h| h.0.clone());
                let groups: Vec<(String, bool, String)> = self
                    .groups
                    .iter()
                    .take(500)
                    .map(|g| (g.name.clone(), false, g.name.clone()))
                    .collect();
                let weak = this.clone();
                row = row
                    .child(field(
                        "Connection",
                        pick("log-conn", current, conns, this.clone(), Self::set_conn),
                        p,
                    ))
                    .child(field("Region", boxed(&self.region, 130.), p))
                    .child(field("Log group (or prefix)", boxed(&self.group, 240.), p))
                    .child(
                        ui::button("log-groups", "List groups", Kind::Secondary, p)
                            .on_click(cx.listener(|t, _, _, cx| t.load_groups(cx))),
                    )
                    .when(!groups.is_empty(), |d| {
                        d.child(
                            gpui_kit::component::button::Button::new("log-group-pick")
                                .outline()
                                .small()
                                .label(format!("{} ▾", plural(self.groups.len(), "group")))
                                .dropdown_menu(move |mut menu, _, _| {
                                    for (title, _, name) in &groups {
                                        let weak = weak.clone();
                                        let name = name.clone();
                                        menu =
                                            menu.item(PopupMenuItem::new(title.clone()).on_click(
                                                move |_, window, cx| {
                                                    let _ = weak.update(cx, |t, cx| {
                                                        t.set_group(name.clone(), window, cx)
                                                    });
                                                },
                                            ));
                                    }
                                    menu
                                }),
                        )
                    })
                    .child(field("Pattern", boxed(&self.pattern, 200.), p))
                    .child(field("Minutes back", boxed(&self.since, 60.), p));
            }
            SrcKind::LogAnalytics => {
                let mut conns =
                    vec![("Azure CLI (az login)".to_owned(), self.conn.is_none(), None)];
                conns.extend(self.conns(CloudProvider::Azure));
                let current = conns
                    .iter()
                    .find(|h| h.1)
                    .map_or_else(|| "Azure CLI (az login)".to_owned(), |h| h.0.clone());
                let spaces: Vec<(String, bool, String)> = self
                    .workspaces
                    .iter()
                    .map(|w| {
                        (
                            format!("{} ({}, {})", w.name, w.resource_group, w.location),
                            self.workspace.as_ref() == Some(&w.customer_id),
                            w.customer_id.clone(),
                        )
                    })
                    .collect();
                let ws_label = self
                    .workspaces
                    .iter()
                    .find(|w| self.workspace.as_ref() == Some(&w.customer_id))
                    .map_or_else(|| "Choose a workspace".to_owned(), |w| w.name.clone());
                row = row
                    .child(field(
                        "Sign-in",
                        pick("log-az", current, conns, this.clone(), Self::set_conn),
                        p,
                    ))
                    .child(
                        ui::button("log-ws-load", "List workspaces", Kind::Secondary, p)
                            .on_click(cx.listener(|t, _, _, cx| t.load_workspaces(cx))),
                    )
                    .when(!spaces.is_empty(), |d| {
                        d.child(field(
                            "Workspace",
                            pick(
                                "log-ws",
                                ws_label,
                                spaces,
                                this.clone(),
                                Self::set_workspace,
                            ),
                            p,
                        ))
                    })
                    .child(field("KQL query", boxed(&self.query, 380.), p))
                    .child(field("Minutes back", boxed(&self.since, 60.), p));
            }
        }
        row = row.child(if self.stream.is_some() {
            ui::button("log-stop", "Stop", Kind::Secondary, p)
                .on_click(cx.listener(|t, _, _, cx| t.stop(cx)))
        } else {
            ui::button("log-start", "Start", Kind::Primary, p)
                .on_click(cx.listener(|t, _, _, cx| t.start(cx)))
        });
        div()
            .flex_none()
            .px(rpx(12.))
            .py(rpx(8.))
            .border_b_1()
            .border_color(p.bd)
            .child(row)
            .into_any_element()
    }

    fn render_bar(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let this = cx.entity().downgrade();
        let levels = [
            None,
            Some(Level::Fatal),
            Some(Level::Error),
            Some(Level::Warn),
            Some(Level::Info),
            Some(Level::Debug),
        ];
        let items: Vec<(String, bool, Option<Level>)> = levels
            .iter()
            .map(|l| {
                (
                    l.map_or("All levels".to_owned(), |l| {
                        format!("{} and above", l.label())
                    }),
                    self.min_level == *l,
                    *l,
                )
            })
            .collect();
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(8.))
            .px(rpx(12.))
            .py(rpx(6.))
            .border_b_1()
            .border_color(p.bd)
            .child(
                input_box(p).w(rpx(220.)).child(
                    Input::new(&self.filter)
                        .appearance(false)
                        .text_size(ts::BODY),
                ),
            )
            .child(
                input_box(p).w(rpx(220.)).child(
                    Input::new(&self.highlight)
                        .appearance(false)
                        .text_size(ts::BODY),
                ),
            )
            .child(pick(
                "log-level",
                self.min_level
                    .map_or("All levels".to_owned(), |l| format!("{}+", l.label())),
                items,
                this,
                Self::set_min_level,
            ))
            .child(
                ui::checkbox("log-follow", self.follow, "Follow", p).on_click(cx.listener(
                    |t, _, _, cx| {
                        t.follow = !t.follow;
                        cx.notify();
                    },
                )),
            )
            .child(div().flex_1())
            .child(div().text_size(ts::SMALL).text_color(p.fg3).child(format!(
                "{} of {}{}",
                ui::thousands(self.shown.len() as u64),
                plural(self.lines.len(), "line"),
                if self.held.is_empty() {
                    String::new()
                } else {
                    format!(" · {} held", ui::thousands(self.held.len() as u64))
                }
            )))
            .when(self.stream.is_some(), |d| {
                d.child(
                    ui::button(
                        "log-pause",
                        if self.paused { "Resume" } else { "Pause" },
                        Kind::Secondary,
                        p,
                    )
                    .on_click(cx.listener(|t, _, _, cx| t.toggle_pause(cx))),
                )
            })
            .child(
                ui::button("log-copy", "Copy", Kind::Ghost, p)
                    .on_click(cx.listener(|t, _, _, cx| t.copy_all(cx))),
            )
            .child(
                ui::button("log-export", "Export…", Kind::Ghost, p)
                    .on_click(cx.listener(|t, _, _, cx| t.export(cx))),
            )
            .into_any_element()
    }

    fn level_color(l: Level, p: &Palette) -> Hsla {
        match l {
            Level::Fatal | Level::Error => p.prod,
            Level::Warn => p.stg,
            Level::Info => p.acc,
            Level::Debug | Level::Trace => p.fg3,
            Level::Unknown => p.fg3,
        }
    }

    fn render_lines(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        if self.shown.is_empty() {
            return div()
                .flex_1()
                .p(rpx(16.))
                .text_size(ts::BODY)
                .text_color(p.fg3)
                .child(if self.lines.is_empty() {
                    if self.stream.is_some() {
                        "Waiting for lines…"
                    } else {
                        "Pick a source and press Start."
                    }
                } else {
                    "No line matches the filter."
                })
                .into_any_element();
        }
        let terms: Vec<String> = Self::value(&self.highlight, cx)
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        let p2 = *p;
        let mark = HighlightStyle {
            background_color: Some(p.stg.opacity(0.35)),
            ..HighlightStyle::default()
        };
        uniform_list(
            "log-lines",
            self.shown.len(),
            cx.processor(move |t, range: Range<usize>, _w, _cx| {
                let p = &p2;
                range
                    .filter_map(|i| {
                        let abs = *t.shown.get(i)?;
                        let l = t.lines.get(abs.checked_sub(t.dropped)?)?;
                        let msg = one_line(&l.message);
                        let hl: Vec<(Range<usize>, HighlightStyle)> =
                            highlight_ranges(&msg, &terms)
                                .into_iter()
                                .map(|r| (r, mark))
                                .collect();
                        let text = StyledText::new(msg).with_highlights(hl);
                        Some(
                            div()
                                .h(rpx(ROW_H))
                                .flex()
                                .items_center()
                                .gap(rpx(8.))
                                .px(rpx(12.))
                                .font_family(MONO)
                                .text_size(ts::LABEL)
                                .hover(|s| s.bg(p.hover))
                                .child(
                                    div()
                                        .w(rpx(150.))
                                        .flex_none()
                                        .text_color(p.fg3)
                                        .child(l.ts_ms.map(iso).unwrap_or_default()),
                                )
                                .child(
                                    div()
                                        .w(rpx(44.))
                                        .flex_none()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(Self::level_color(l.level, p))
                                        .child(if l.level == Level::Unknown {
                                            ""
                                        } else {
                                            l.level.label()
                                        }),
                                )
                                .when(!l.source.is_empty(), |d| {
                                    d.child(
                                        div()
                                            .max_w(rpx(140.))
                                            .flex_none()
                                            .truncate()
                                            .text_color(p.fg2)
                                            .child(l.source.clone()),
                                    )
                                })
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(match l.level {
                                            Level::Fatal | Level::Error => p.prod,
                                            _ => p.fg,
                                        })
                                        .child(text),
                                )
                                .into_any_element(),
                        )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.scroll)
        .flex_1()
        .into_any_element()
    }
}

impl Render for LogsTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        div()
            .id("log-tab")
            .size_full()
            .flex()
            .flex_col()
            .font_family(SANS)
            .bg(p.bg)
            .text_color(p.fg)
            .child(self.render_source(&p, cx))
            .child(self.render_bar(&p, cx))
            .children(
                self.status
                    .as_ref()
                    .map(|(ok, s)| status_line(*ok, s.clone(), &p).flex_none()),
            )
            .when(self.dropped > 0, |d| {
                d.child(
                    status_line(
                        true,
                        format!(
                            "Showing the latest {} lines; {} older dropped",
                            ui::thousands(MAX_LINES as u64),
                            ui::thousands(self.dropped as u64)
                        ),
                        &p,
                    )
                    .flex_none(),
                )
            })
            .child(self.render_lines(&p, cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_merge_and_ignore_case() {
        let terms = vec!["err".to_owned(), "ERROR".to_owned(), "db".to_owned()];
        let r = highlight_ranges("Error talking to DB; error again", &terms);
        assert_eq!(r, [0..5, 17..19, 21..26]);
        assert!(highlight_ranges("nothing", &terms).is_empty());
        assert!(highlight_ranges("x", &[String::new()]).is_empty());
        // Non-ASCII text keeps byte offsets.
        let r = highlight_ranges("é error", &["error".to_owned()]);
        assert_eq!((r.len(), r.first()), (1, Some(&(3..8))));
    }

    #[test]
    fn multi_line_messages_show_on_one_line() {
        assert_eq!(one_line("a\nb\n"), "a ⏎ b");
    }
}
