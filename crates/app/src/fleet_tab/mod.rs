//! The Fleet tab: one tool (run a command, dashboard, push a file, manage SSH keys) on a
//! selection of Hosts (a folder, a tag, or Hosts picked one by one).
//!
//! The tab only sends [`FleetCommand`]s and folds the runtime's [`FleetEvent`]s into its
//! rows ([`model::Rows`]); all SSH work happens on the core runtime. Acting on
//! Production Hosts (run, push, add or remove a key) asks for the typed confirmation
//! first, and the core checks it again. The dashboard probe and key audits only read.

pub mod model;

use std::time::{Duration, Instant};

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, EventEmitter, FontWeight, Global,
    InteractiveElement as _, IntoElement, ParentElement as _, PathPromptOptions, Render,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
    px, uniform_list,
};
use switchyard_core::fleet::{
    CONCURRENCY_KEY, DEFAULT_CONCURRENCY, DEFAULT_TIMEOUT_SECS, FleetAction, FleetCommand,
    FleetEvent, HostStatus, KeyOp, Overwrite,
};
use switchyard_core::store::{EnvironmentLabel, Host, Profile, ProfileId};
use switchyard_core::{Command, RuntimeHandle};

use crate::app_state::next_id;
use crate::appearance::{rpx, ts};
use crate::theme::{MONO, Palette, SANS, palette};
use crate::ui::{self, Kind};
use crate::workspace::{Tab, Workspace};
use model::{DashCol, HostRow, Pick, Rows};

/// Row height of the grids.
const ROW_H: f32 = 26.;
/// Dashboard refresh intervals offered, seconds.
const INTERVALS: [u64; 3] = [30, 60, 300];

/// What a Fleet tab does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Run a command or macro.
    Run,
    /// Health of every Host.
    Dashboard,
    /// Upload a file or folder.
    Push,
    /// SSH keys in `authorized_keys`.
    Keys,
}

impl Mode {
    /// Tab and header name.
    pub fn title(self) -> &'static str {
        match self {
            Mode::Run => "Run",
            Mode::Dashboard => "Dashboard",
            Mode::Push => "Push",
            Mode::Keys => "Keys",
        }
    }
}

/// Events for the workspace.
pub enum FleetTabEvent {
    /// Open a terminal on this Host.
    OpenTerminal(ProfileId),
}

/// Hosts at once for runs started from the app (the `fleet.concurrency` setting).
#[derive(Clone, Copy)]
pub struct FleetConcurrency(pub usize);

impl Global for FleetConcurrency {}

/// The setting's value (default 8).
pub fn concurrency(cx: &gpui_kit::App) -> usize {
    cx.try_global::<FleetConcurrency>()
        .map_or(DEFAULT_CONCURRENCY, |c| c.0)
}

/// An action waiting for confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Pending {
    Run(FleetAction),
    Push,
    Keys(KeyOp),
}

/// A Fleet tab.
pub struct FleetTab {
    core: RuntimeHandle,
    /// What it does.
    pub mode: Mode,
    /// Which Hosts.
    pub pick: Pick,
    /// Every saved Host (for the picker).
    all_hosts: Vec<Host>,
    rows: Rows,
    command: Entity<InputState>,
    timeout: Entity<InputState>,
    parallel: Entity<InputState>,
    local: Entity<InputState>,
    remote_dir: Entity<InputState>,
    chmod: Entity<InputState>,
    overwrite: Overwrite,
    key: Entity<InputState>,
    confirm: Entity<InputState>,
    pending: Option<Pending>,
    failed_only: bool,
    differs_only: bool,
    expanded: Option<ProfileId>,
    picker_open: bool,
    /// Dashboard refresh interval (`None`: paused).
    interval: Option<u64>,
    last_probe: Option<Instant>,
    sort: (DashCol, bool),
    _ticker: Task<()>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<FleetTabEvent> for FleetTab {}

impl FleetTab {
    /// A tab doing `mode` on `pick` among `profiles`.
    pub fn new(
        core: RuntimeHandle,
        mode: Mode,
        pick: Pick,
        profiles: &[Profile],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let parallel_now = concurrency(cx).to_string();
        let mut input = |ph: &str, value: &str| {
            let (ph, value) = (ph.to_owned(), value.to_owned());
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(ph)
                    .default_value(value)
            })
        };
        let command = input("Command to run on every Host (Enter runs)", "");
        let timeout = input("60", &DEFAULT_TIMEOUT_SECS.to_string());
        let parallel = input("8", &parallel_now);
        let local = input("Local file or folder", "");
        let remote_dir = input("Remote folder, e.g. /tmp or ~/app", "");
        let chmod = input("chmod (optional, e.g. 644)", "");
        let key = input("ssh-ed25519 AAAA… user@host (one public key line)", "");
        let confirm = input("Type PRODUCTION to confirm", "");
        let subs = vec![
            cx.subscribe_in(&command, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.run_command(window, cx);
                }
            }),
            cx.subscribe_in(
                &confirm,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter { .. } => this.confirm_pending(window, cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            ),
        ];
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let alive = this.update(cx, |t, cx| t.tick(cx));
                if alive.is_err() {
                    return;
                }
            }
        });
        let mut t = Self {
            core,
            mode,
            pick,
            all_hosts: Vec::new(),
            rows: Rows::default(),
            command,
            timeout,
            parallel,
            local,
            remote_dir,
            chmod,
            overwrite: Overwrite::Replace,
            key,
            confirm,
            pending: None,
            failed_only: false,
            differs_only: false,
            expanded: None,
            picker_open: false,
            interval: Some(INTERVALS[0]),
            last_probe: None,
            sort: (DashCol::Health, true),
            _ticker: ticker,
            _subs: subs,
        };
        t.set_profiles(profiles);
        if mode == Mode::Dashboard {
            t.probe();
        }
        t
    }

    /// The saved profiles changed.
    pub fn set_profiles(&mut self, profiles: &[Profile]) {
        self.all_hosts = profiles
            .iter()
            .filter_map(|p| match p {
                Profile::Host(h) => Some(h.clone()),
                _ => None,
            })
            .collect();
        let hosts = self.pick.hosts(self.all_hosts.iter());
        self.rows.set_hosts(&hosts);
    }

    /// The selected Hosts.
    fn selected(&self) -> Vec<&Host> {
        self.pick.hosts(self.all_hosts.iter())
    }

    /// Tab title.
    pub fn title(&self) -> String {
        format!(
            "Fleet {} · {}",
            self.mode.title(),
            self.pick.label(self.rows.rows.len())
        )
    }

    /// The most exposed environment of the selection.
    pub fn environment(&self) -> Option<EnvironmentLabel> {
        let envs: Vec<EnvironmentLabel> = self.rows.rows.iter().map(|r| r.env).collect();
        EnvironmentLabel::ALL.into_iter().find(|e| envs.contains(e))
    }

    /// Stop what runs (the tab is closing).
    pub fn shutdown(&mut self) {
        for run in [self.rows.run, self.rows.probe_run].into_iter().flatten() {
            self.core.send(Command::Fleet(FleetCommand::Cancel { run }));
        }
    }

    /// A Fleet event; ignored unless it belongs to this tab.
    pub fn on_event(&mut self, ev: &FleetEvent, cx: &mut Context<Self>) {
        if self.rows.apply(ev) {
            cx.notify();
        }
    }

    fn set_pick(&mut self, pick: Pick, cx: &mut Context<Self>) {
        self.pick = pick;
        let hosts = self.pick.hosts(self.all_hosts.iter());
        self.rows.set_hosts(&hosts);
        self.pending = None;
        if self.mode == Mode::Dashboard {
            self.probe();
        }
        cx.notify();
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.mode = mode;
        self.pending = None;
        self.expanded = None;
        if mode == Mode::Dashboard && self.rows.probe_run.is_none() {
            self.probe();
        }
        cx.notify();
    }

    /// Dashboard refresh timer.
    fn tick(&mut self, cx: &mut Context<Self>) {
        let Some(secs) = self.interval else { return };
        if self.mode != Mode::Dashboard || self.rows.probe_run.is_some() {
            return;
        }
        if self
            .last_probe
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(secs))
        {
            self.probe();
            cx.notify();
        }
    }

    fn ids(&self) -> Vec<ProfileId> {
        self.rows.rows.iter().map(|r| r.id.clone()).collect()
    }

    fn probe(&mut self) {
        if self.rows.rows.is_empty() {
            return;
        }
        let run = next_id();
        self.rows.probe_run = Some(run);
        self.last_probe = Some(Instant::now());
        self.core.send(Command::Fleet(FleetCommand::Probe {
            run,
            hosts: self.ids(),
        }));
    }

    fn number(&self, input: &Entity<InputState>, default: u64, cx: &Context<Self>) -> u64 {
        input
            .read(cx)
            .value()
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .unwrap_or(default)
    }

    /// Ask for confirmation when needed, else start `action`.
    fn request(&mut self, action: Pending, window: &mut Window, cx: &mut Context<Self>) {
        if self.rows.running || self.rows.rows.is_empty() {
            return;
        }
        let production = self.needs_typed(&action);
        let removing = matches!(action, Pending::Keys(KeyOp::Remove(_)));
        if production || removing {
            self.pending = Some(action);
            self.confirm.update(cx, |i, cx| i.set_value("", window, cx));
            if production {
                self.confirm.update(cx, |i, cx| i.focus(window, cx));
            }
            cx.notify();
            return;
        }
        self.start(action, false, cx);
    }

    /// Whether `action` needs the typed Production confirmation: it changes something
    /// and the selection holds Production Hosts (reads such as audits do not).
    fn needs_typed(&self, action: &Pending) -> bool {
        !matches!(action, Pending::Keys(KeyOp::Audit(_)))
            && model::production_count(&self.selected()) > 0
    }

    fn confirm_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(production) = self.pending.as_ref().map(|a| self.needs_typed(a)) else {
            return;
        };
        if production && !model::confirmation_ok(&self.confirm.read(cx).value()) {
            return;
        }
        let Some(action) = self.pending.take() else {
            return;
        };
        self.confirm.update(cx, |i, cx| i.set_value("", window, cx));
        self.start(action, production, cx);
    }

    fn start(&mut self, action: Pending, confirmed: bool, cx: &mut Context<Self>) {
        let run = next_id();
        let hosts = self.ids();
        let parallel = self.number(&self.parallel, concurrency(cx) as u64, cx) as usize;
        if parallel != concurrency(cx) {
            // The field is the setting: remember it for the next run.
            cx.set_global(FleetConcurrency(parallel));
            self.core.send(Command::SetSetting {
                key: CONCURRENCY_KEY.into(),
                value: serde_json::json!(parallel),
            });
        }
        let concurrency = Some(parallel);
        let cmd = match action {
            Pending::Run(action) => FleetCommand::Run {
                run,
                hosts,
                action,
                concurrency,
                timeout_secs: self.number(&self.timeout, DEFAULT_TIMEOUT_SECS, cx),
                confirmed,
            },
            Pending::Push => {
                let chmod = self.chmod.read(cx).value().trim().to_owned();
                FleetCommand::Push {
                    run,
                    hosts,
                    local: self.local.read(cx).value().trim().into(),
                    remote_dir: self.remote_dir.read(cx).value().trim().to_owned(),
                    overwrite: self.overwrite,
                    chmod: (!chmod.is_empty()).then_some(chmod),
                    concurrency,
                    confirmed,
                }
            }
            Pending::Keys(op) => FleetCommand::Keys {
                run,
                hosts,
                op,
                concurrency,
                confirmed,
            },
        };
        self.rows.start(run);
        self.expanded = None;
        self.core.send(Command::Fleet(cmd));
        cx.notify();
    }

    fn run_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.command.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.request(Pending::Run(FleetAction::Command(text)), window, cx);
    }

    fn key_text(&self, cx: &Context<Self>) -> String {
        self.key.read(cx).value().trim().to_owned()
    }

    fn cancel_all(&mut self, cx: &mut Context<Self>) {
        if let Some(run) = self.rows.run.filter(|_| self.rows.running) {
            self.core.send(Command::Fleet(FleetCommand::Cancel { run }));
        }
        cx.notify();
    }

    fn pick_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: false,
            prompt: Some("Push".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = chosen.await else {
                return;
            };
            let Some(path) = paths.pop() else { return };
            let path = path.to_string_lossy().into_owned();
            let _ = this.update_in(cx, |this, window, cx| {
                this.local.update(cx, |i, cx| i.set_value(path, window, cx));
                cx.notify();
            });
        })
        .detach();
    }

    // ------------------------------------------------------------------ render

    fn render_header(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mode_btn = |m: Mode| -> (SharedString, bool, ui::OnClick) {
            let this = cx.entity().downgrade();
            (
                m.title().into(),
                self.mode == m,
                Box::new(move |_, _, cx| {
                    let _ = this.update(cx, |t, cx| t.set_mode(m, cx));
                }),
            )
        };
        let production = model::production_count(&self.selected());
        let (ok, failed, other, busy) = self.rows.tally();
        div()
            .flex_none()
            .h(rpx(38.))
            .flex()
            .items_center()
            .gap(rpx(10.))
            .px(rpx(12.))
            .border_b_1()
            .border_color(p.bd)
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_size(ts::BASE)
                    .child("Fleet"),
            )
            .child(ui::segmented(
                "fleet-mode",
                [Mode::Run, Mode::Dashboard, Mode::Push, Mode::Keys]
                    .into_iter()
                    .map(mode_btn)
                    .collect(),
                20.,
                p,
            ))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(MONO)
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .child(format!(
                        "{} · {} Hosts",
                        self.pick.label(self.rows.rows.len()),
                        self.rows.rows.len()
                    )),
            )
            .when(production > 0, |d| {
                d.child(
                    div()
                        .flex_none()
                        .px(rpx(5.))
                        .rounded(px(3.))
                        .border_1()
                        .border_color(p.prod)
                        .text_size(ts::TINY_PLUS)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(p.prod)
                        .child(format!("{production} PRODUCTION")),
                )
            })
            .child(div().flex_1())
            .when(
                self.mode != Mode::Dashboard && ok + failed + other + busy > 0,
                |d| {
                    d.child(
                        div()
                            .flex_none()
                            .font_family(MONO)
                            .text_size(ts::SMALL)
                            .text_color(p.fg2)
                            .child(format!(
                                "{ok} ok · {failed} failed{}{}",
                                if other > 0 {
                                    format!(" · {other} other")
                                } else {
                                    String::new()
                                },
                                if busy > 0 {
                                    format!(" · {busy} running")
                                } else {
                                    String::new()
                                }
                            )),
                    )
                },
            )
            .when(self.rows.running, |d| {
                d.child(
                    ui::button("fleet-cancel", "Cancel all", Kind::Destructive, p)
                        .h(rpx(24.))
                        .on_click(cx.listener(|t, _, _, cx| t.cancel_all(cx))),
                )
            })
            .into_any_element()
    }

    /// Folder, tag and "choose" chips.
    fn render_targets(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let mut folders: Vec<String> = Vec::new();
        let mut tags: Vec<String> = Vec::new();
        for h in &self.all_hosts {
            if let Some(f) = h.folder.as_deref().map(str::trim).filter(|f| !f.is_empty())
                && !folders.iter().any(|x| x == f)
            {
                folders.push(f.to_owned());
            }
            for t in switchyard_core::store::tags::normalize_tags(&h.tags) {
                if !tags.contains(&t) {
                    tags.push(t);
                }
            }
        }
        folders.sort_by_key(|f| f.to_lowercase());
        tags.sort();
        let mut picks: Vec<(String, Pick)> = vec![("All".into(), Pick::All)];
        picks.extend(
            folders
                .into_iter()
                .map(|f| (format!("▣ {f}"), Pick::Folder(f))),
        );
        picks.extend(tags.into_iter().map(|t| (format!("# {t}"), Pick::Tag(t))));
        let chosen = matches!(self.pick, Pick::Hosts(_));
        div()
            .id("fleet-targets")
            .flex_none()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(rpx(6.))
            .px(rpx(12.))
            .py(rpx(6.))
            .border_b_1()
            .border_color(p.bd)
            .child(ui::caption("HOSTS", p))
            .children(picks.into_iter().enumerate().map(|(i, (label, pick))| {
                let on = self.pick == pick;
                div()
                    .id(("fleet-pick", i))
                    .flex_none()
                    .px(rpx(8.))
                    .py(rpx(2.))
                    .rounded(px(10.))
                    .border_1()
                    .border_color(if on { p.acc } else { p.bd2 })
                    .bg(if on { p.sel } else { p.bg })
                    .text_size(ts::SMALL)
                    .cursor_pointer()
                    .hover(|s| s.border_color(p.fg3))
                    .on_click(cx.listener(move |t, _, _, cx| t.set_pick(pick.clone(), cx)))
                    .child(label)
            }))
            .child(
                div()
                    .id("fleet-pick-choose")
                    .flex_none()
                    .px(rpx(8.))
                    .py(rpx(2.))
                    .rounded(px(10.))
                    .border_1()
                    .border_color(if chosen || self.picker_open {
                        p.acc
                    } else {
                        p.bd2
                    })
                    .text_size(ts::SMALL)
                    .cursor_pointer()
                    .on_click(cx.listener(|t, _, _, cx| {
                        t.picker_open = !t.picker_open;
                        cx.notify();
                    }))
                    .child(if self.picker_open {
                        "Done choosing"
                    } else {
                        "Choose Hosts…"
                    }),
            )
            .into_any_element()
    }

    /// Every Host with a checkbox (virtualized).
    fn render_picker(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let count = self.all_hosts.len();
        let p2 = *p;
        div()
            .flex_none()
            .h(rpx((count.min(8) as f32).max(1.) * ROW_H + 8.))
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(p.bd)
            .child(
                uniform_list(
                    "fleet-picker",
                    count,
                    cx.processor(move |this, range: std::ops::Range<usize>, _w, cx| {
                        let p = &p2;
                        let selected: Vec<ProfileId> = this.ids();
                        range
                            .map(|i| {
                                let h = &this.all_hosts[i];
                                let on = selected.contains(&h.id);
                                let id = h.id.clone();
                                div()
                                    .id(("fleet-picker-row", i))
                                    .h(rpx(ROW_H))
                                    .flex()
                                    .items_center()
                                    .gap(rpx(8.))
                                    .px(rpx(14.))
                                    .hover(|s| s.bg(p.hover))
                                    .on_click(cx.listener(move |t, _, _, cx| {
                                        let mut ids = t.ids();
                                        if let Some(at) = ids.iter().position(|x| x == &id) {
                                            ids.remove(at);
                                        } else {
                                            ids.push(id.clone());
                                        }
                                        t.set_pick(Pick::Hosts(ids), cx);
                                    }))
                                    .child(ui::checkbox(("fleet-picker-check", i), on, "", p))
                                    .child(ui::dot(p.env(h.environment), 6.))
                                    .child(
                                        div()
                                            .w(rpx(180.))
                                            .truncate()
                                            .text_size(ts::BODY)
                                            .child(h.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .font_family(MONO)
                                            .text_size(ts::SMALL)
                                            .text_color(p.fg3)
                                            .child(format!("{}  {}", h.address, h.tags.join(" "))),
                                    )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1(),
            )
            .into_any_element()
    }

    fn render_form(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let label = |text: &'static str| {
            div()
                .flex_none()
                .text_size(ts::SMALL)
                .text_color(p.fg3)
                .child(text)
        };
        let busy = self.rows.running;
        let form = div()
            .flex_none()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(rpx(8.))
            .px(rpx(12.))
            .py(rpx(8.))
            .border_b_1()
            .border_color(p.bd);
        match self.mode {
            Mode::Run => {
                let macros = crate::terminal_settings::macros(cx);
                form.child(
                    div()
                        .flex_1()
                        .min_w(rpx(260.))
                        .child(Input::new(&self.command).text_size(ts::BODY)),
                )
                .child(label("Timeout s"))
                .child(div().w(rpx(56.)).child(Input::new(&self.timeout).text_size(ts::BODY)))
                .child(label("At once"))
                .child(div().w(rpx(48.)).child(Input::new(&self.parallel).text_size(ts::BODY)))
                .child(
                    ui::button("fleet-run", "Run", Kind::Primary, p)
                        .when(busy, |b| b.opacity(0.5))
                        .on_click(cx.listener(|t, _, w, cx| t.run_command(w, cx))),
                )
                .when(!macros.is_empty(), |d| {
                    d.child(
                        div()
                            .w_full()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(rpx(6.))
                            .child(label("Run a macro"))
                            .children(macros.into_iter().enumerate().map(|(i, m)| {
                                let id = m.id.clone();
                                ui::button(("fleet-macro", i), m.name.clone(), Kind::Secondary, p)
                                    .h(rpx(22.))
                                    .when(busy, |b| b.opacity(0.5))
                                    .on_click(cx.listener(move |t, _, w, cx| {
                                        t.request(
                                            Pending::Run(FleetAction::Macro(id.clone())),
                                            w,
                                            cx,
                                        )
                                    }))
                            })),
                    )
                })
                .into_any_element()
            }
            Mode::Dashboard => {
                let interval_btn = |secs: Option<u64>| -> (SharedString, bool, ui::OnClick) {
                    let this = cx.entity().downgrade();
                    (
                        match secs {
                            Some(s) if s >= 60 => format!("{} min", s / 60).into(),
                            Some(s) => format!("{s} s").into(),
                            None => "Paused".into(),
                        },
                        self.interval == secs,
                        Box::new(move |_, _, cx| {
                            let _ = this.update(cx, |t, cx| {
                                t.interval = secs;
                                cx.notify();
                            });
                        }),
                    )
                };
                let flagged = self
                    .rows
                    .rows
                    .iter()
                    .filter(|r| !r.flags().is_empty())
                    .count();
                form.child(label("Refresh"))
                    .child(ui::segmented(
                        "fleet-interval",
                        INTERVALS
                            .iter()
                            .map(|s| Some(*s))
                            .chain([None])
                            .map(interval_btn)
                            .collect(),
                        20.,
                        p,
                    ))
                    .child(if self.rows.probe_run.is_some() {
                        ui::shimmer(70., p)
                    } else {
                        ui::button("fleet-probe", "Refresh now", Kind::Secondary, p)
                            .h(rpx(24.))
                            .on_click(cx.listener(|t, _, _, cx| {
                                t.probe();
                                cx.notify();
                            }))
                            .into_any_element()
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(ts::SMALL)
                            .text_color(if flagged > 0 { p.prod } else { p.fg3 })
                            .child(format!(
                                "{flagged} of {} need attention · click a row for a terminal",
                                self.rows.rows.len()
                            )),
                    )
                    .into_any_element()
            }
            Mode::Push => {
                let ow = |o: Overwrite, name: &'static str| -> (SharedString, bool, ui::OnClick) {
                    let this = cx.entity().downgrade();
                    (
                        name.into(),
                        self.overwrite == o,
                        Box::new(move |_, _, cx| {
                            let _ = this.update(cx, |t, cx| {
                                t.overwrite = o;
                                cx.notify();
                            });
                        }),
                    )
                };
                form.child(
                    div()
                        .flex_1()
                        .min_w(rpx(220.))
                        .child(Input::new(&self.local).text_size(ts::BODY)),
                )
                .child(
                    ui::button("fleet-browse", "Choose…", Kind::Secondary, p)
                        .on_click(cx.listener(|t, _, w, cx| t.pick_local(w, cx))),
                )
                .child(label("→"))
                .child(
                    div()
                        .w(rpx(200.))
                        .child(Input::new(&self.remote_dir).text_size(ts::BODY)),
                )
                .child(div().w(rpx(150.)).child(Input::new(&self.chmod).text_size(ts::BODY)))
                .child(label("If it exists"))
                .child(ui::segmented(
                    "fleet-overwrite",
                    vec![
                        ow(Overwrite::Replace, "Replace"),
                        ow(Overwrite::Skip, "Skip"),
                        ow(Overwrite::KeepBoth, "Keep both"),
                    ],
                    20.,
                    p,
                ))
                .child(label("At once"))
                .child(div().w(rpx(48.)).child(Input::new(&self.parallel).text_size(ts::BODY)))
                .child(
                    ui::button("fleet-push", "Push", Kind::Primary, p)
                        .when(busy, |b| b.opacity(0.5))
                        .on_click(cx.listener(|t, _, w, cx| t.request(Pending::Push, w, cx))),
                )
                .into_any_element()
            }
            Mode::Keys => form
                .child(
                    div()
                        .flex_1()
                        .min_w(rpx(320.))
                        .child(Input::new(&self.key).text_size(ts::BODY)),
                )
                .child(
                    ui::button("fleet-audit", "Audit", Kind::Secondary, p)
                        .when(busy, |b| b.opacity(0.5))
                        .on_click(cx.listener(|t, _, w, cx| {
                            let k = t.key_text(cx);
                            t.request(
                                Pending::Keys(KeyOp::Audit((!k.is_empty()).then_some(k))),
                                w,
                                cx,
                            )
                        })),
                )
                .child(
                    ui::button("fleet-key-add", "Add key", Kind::Primary, p)
                        .when(busy, |b| b.opacity(0.5))
                        .on_click(cx.listener(|t, _, w, cx| {
                            let k = t.key_text(cx);
                            if !k.is_empty() {
                                t.request(Pending::Keys(KeyOp::Add(k)), w, cx)
                            }
                        })),
                )
                .child(
                    ui::button("fleet-key-remove", "Remove key…", Kind::Secondary, p)
                        .when(busy, |b| b.opacity(0.5))
                        .on_click(cx.listener(|t, _, w, cx| {
                            let k = t.key_text(cx);
                            if !k.is_empty() {
                                t.request(Pending::Keys(KeyOp::Remove(k)), w, cx)
                            }
                        })),
                )
                .child(
                    div()
                        .w_full()
                        .text_size(ts::SMALL)
                        .text_color(p.fg3)
                        .child(
                            "Audit lists the keys of every Host (with a key above: who has it). \
                             Changes back up ~/.ssh/authorized_keys to authorized_keys.switchyard-bak \
                             first; the key Switchyard signed in with is never removed.",
                        ),
                )
                .into_any_element(),
        }
    }

    fn render_confirm(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pending = self.pending.as_ref()?;
        let selected = self.selected();
        let production = if self.needs_typed(pending) {
            model::production_count(&selected)
        } else {
            0
        };
        let ok = production == 0 || model::confirmation_ok(&self.confirm.read(cx).value());
        let what = match pending {
            Pending::Run(FleetAction::Command(c)) => format!("Run `{c}`"),
            Pending::Run(FleetAction::Macro(_)) => "Run the macro".to_owned(),
            Pending::Push => format!(
                "Push {} to {}",
                self.local.read(cx).value(),
                self.remote_dir.read(cx).value()
            ),
            Pending::Keys(KeyOp::Remove(_)) => "Remove this key from authorized_keys".to_owned(),
            Pending::Keys(KeyOp::Add(_)) => "Add this key to authorized_keys".to_owned(),
            Pending::Keys(KeyOp::Audit(_)) => "Audit authorized_keys".to_owned(),
        };
        Some(
            div()
                .flex_none()
                .flex()
                .flex_col()
                .gap(rpx(6.))
                .mx(rpx(12.))
                .mt(rpx(8.))
                .px(rpx(10.))
                .py(rpx(8.))
                .rounded(px(6.))
                .bg(p.prod.opacity(0.08))
                .border_1()
                .border_color(p.prod.opacity(0.5))
                .text_size(ts::BODY)
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!("{what} on {} Hosts?", selected.len())),
                )
                .when(production > 0, |d| {
                    d.child(div().text_color(p.prod).child(format!(
                        "{production} of them are Production: type {} to confirm.",
                        model::CONFIRM_WORD
                    )))
                    .child(
                        div()
                            .w(rpx(260.))
                            .child(Input::new(&self.confirm).text_size(ts::BODY)),
                    )
                })
                .child(
                    div()
                        .flex()
                        .gap(rpx(8.))
                        .child(
                            ui::button("fleet-confirm-no", "Cancel", Kind::Secondary, p).on_click(
                                cx.listener(|t, _, _, cx| {
                                    t.pending = None;
                                    cx.notify();
                                }),
                            ),
                        )
                        .child(
                            ui::button(
                                "fleet-confirm-yes",
                                "Confirm",
                                if ok {
                                    Kind::Destructive
                                } else {
                                    Kind::Secondary
                                },
                                p,
                            )
                            .when(!ok, |b| b.opacity(0.5))
                            .on_click(cx.listener(|t, _, w, cx| t.confirm_pending(w, cx))),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Filters, copy-all and the refusal message.
    fn render_tools(&self, shown: &[usize], p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let copy = self.rows.copy_all(shown);
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(12.))
            .px(rpx(12.))
            .py(rpx(6.))
            .child(
                ui::checkbox("fleet-failed", self.failed_only, "Failed only", p).on_click(
                    cx.listener(|t, _, _, cx| {
                        t.failed_only = !t.failed_only;
                        cx.notify();
                    }),
                ),
            )
            .when(self.mode == Mode::Run, |d| {
                d.child(
                    ui::checkbox(
                        "fleet-differs",
                        self.differs_only,
                        "Differs from majority",
                        p,
                    )
                    .on_click(cx.listener(|t, _, _, cx| {
                        t.differs_only = !t.differs_only;
                        cx.notify();
                    })),
                )
            })
            .child(
                ui::button("fleet-copy", "Copy all", Kind::Ghost, p)
                    .h(rpx(22.))
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                    }),
            )
            .child(div().flex_1())
            .when_some(self.rows.refused.clone(), |d, m| {
                d.child(div().text_size(ts::BODY).text_color(p.prod).child(m))
            })
            .into_any_element()
    }

    /// Column layout of the grid: (header, width; 0 = the rest).
    fn columns(&self) -> Vec<(&'static str, f32)> {
        match self.mode {
            Mode::Run => vec![
                ("HOST", 170.),
                ("STATUS", 96.),
                ("EXIT", 50.),
                ("DURATION", 80.),
                ("FIRST LINE", 0.),
            ],
            Mode::Push => vec![
                ("HOST", 170.),
                ("STATUS", 96.),
                ("PROGRESS", 130.),
                ("RESULT", 0.),
            ],
            Mode::Keys => vec![
                ("HOST", 170.),
                ("STATUS", 96.),
                ("HAS KEY", 70.),
                ("KEYS", 50.),
                ("RESULT", 0.),
            ],
            Mode::Dashboard => DashCol::ALL.iter().map(|(_, h, w)| (*h, *w)).collect(),
        }
    }

    fn cells(&self, r: &HostRow) -> Vec<String> {
        let status = r.status.map_or(String::new(), |s| s.label().to_owned());
        let line = if r.first_line.is_empty() {
            r.error.clone().unwrap_or_default()
        } else {
            r.first_line.clone()
        };
        match self.mode {
            Mode::Run => vec![
                r.name.clone(),
                status,
                r.exit.map(|c| c.to_string()).unwrap_or_default(),
                r.duration_ms
                    .map(|d| ui::duration(Duration::from_millis(d)))
                    .unwrap_or_default(),
                line,
            ],
            Mode::Push => vec![
                r.name.clone(),
                status,
                match r.progress {
                    Some((d, Some(t))) if t > 0 => {
                        format!("{} / {}", ui::bytes(d), ui::bytes(t))
                    }
                    Some((d, _)) => ui::bytes(d),
                    None => String::new(),
                },
                line,
            ],
            Mode::Keys => {
                let (has, n) = match &r.keys {
                    Some(Ok(k)) => (
                        match k.has_key {
                            Some(true) => "yes",
                            Some(false) => "no",
                            None => "",
                        },
                        k.keys.len().to_string(),
                    ),
                    _ => ("", String::new()),
                };
                vec![r.name.clone(), status, has.to_owned(), n, line]
            }
            Mode::Dashboard => DashCol::ALL
                .iter()
                .map(|(c, _, _)| model::dash_cell(r, *c))
                .collect(),
        }
    }

    fn row_color(&self, r: &HostRow, p: &Palette) -> gpui_kit::Hsla {
        if self.mode == Mode::Dashboard {
            return match &r.probe {
                Some(Err(_)) => p.prod,
                Some(Ok(_)) if !r.flags().is_empty() => p.stg,
                _ => p.fg,
            };
        }
        match r.status {
            Some(s) if s.is_failure() => p.prod,
            Some(HostStatus::Ok) => p.fg,
            Some(HostStatus::Running | HostStatus::Connecting) => p.acc,
            _ => p.fg2,
        }
    }

    fn render_grid(&self, shown: Vec<usize>, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let columns = self.columns();
        let cell = |w: f32| {
            let d = div().min_w_0().px(rpx(5.)).truncate();
            if w == 0. {
                d.flex_1().min_w(rpx(140.))
            } else {
                d.w(rpx(w)).flex_none()
            }
        };
        let dash = self.mode == Mode::Dashboard;
        let header = div()
            .flex_none()
            .flex()
            .h(rpx(ROW_H))
            .items_center()
            .px(rpx(8.))
            .border_b_1()
            .border_color(p.bd)
            .text_size(ts::CAPTION_PLUS)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(p.fg3)
            .children(columns.iter().enumerate().map(|(i, (h, w))| {
                let mut label = (*h).to_owned();
                if dash && DashCol::ALL[i].0 == self.sort.0 {
                    label.push_str(if self.sort.1 { " ▾" } else { " ▴" });
                }
                cell(*w)
                    .id(("fleet-head", i))
                    .when(dash, |d| {
                        d.cursor_pointer().on_click(cx.listener(move |t, _, _, cx| {
                            let col = DashCol::ALL[i].0;
                            t.sort = if t.sort.0 == col {
                                (col, !t.sort.1)
                            } else {
                                (col, true)
                            };
                            cx.notify();
                        }))
                    })
                    .child(label)
            }))
            .child(div().w(rpx(66.)).flex_none());
        if self.rows.rows.is_empty() {
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .child(header)
                .child(
                    div()
                        .p(rpx(16.))
                        .text_size(ts::BODY)
                        .text_color(p.fg3)
                        .child("No Hosts selected: pick a folder, a tag or Hosts above."),
                )
                .into_any_element();
        }
        let p2 = *p;
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(header)
            .child(
                uniform_list(
                    "fleet-rows",
                    shown.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, _w, cx| {
                        let p = &p2;
                        let columns = this.columns();
                        range
                            .filter_map(|n| {
                                let i = *shown.get(n)?;
                                let r = this.rows.rows.get(i)?;
                                let id = r.id.clone();
                                let term = r.id.clone();
                                let open = this.expanded.as_ref() == Some(&r.id);
                                let color = this.row_color(r, p);
                                let cells = this.cells(r);
                                let dash = this.mode == Mode::Dashboard;
                                Some(
                                    div()
                                        .id(("fleet-row", n))
                                        .w_full()
                                        .h(rpx(ROW_H))
                                        .flex()
                                        .items_center()
                                        .px(rpx(8.))
                                        .border_b_1()
                                        .border_color(p.line)
                                        .text_size(ts::BODY)
                                        .text_color(color)
                                        .cursor_pointer()
                                        .when(open, |d| d.bg(p.sel))
                                        .when(!open, |d| d.hover(|s| s.bg(p.hover)))
                                        .on_click(cx.listener(move |t, _, _, cx| {
                                            if dash {
                                                cx.emit(FleetTabEvent::OpenTerminal(id.clone()));
                                            } else if t.expanded.as_ref() == Some(&id) {
                                                t.expanded = None;
                                            } else {
                                                t.expanded = Some(id.clone());
                                            }
                                            cx.notify();
                                        }))
                                        .children(
                                            cells.into_iter().zip(columns.iter()).enumerate().map(
                                                |(c, (text, (_, w)))| {
                                                    cell(*w)
                                                        .when(c == 0, |d| {
                                                            d.flex()
                                                                .gap(rpx(6.))
                                                                .items_center()
                                                                .child(ui::dot(p.env(r.env), 6.))
                                                        })
                                                        .when(c > 0, |d| d.font_family(MONO))
                                                        .child(
                                                            div().min_w_0().truncate().child(text),
                                                        )
                                                },
                                            ),
                                        )
                                        .child(
                                            div()
                                                .id(("fleet-term", n))
                                                .w(rpx(66.))
                                                .flex_none()
                                                .text_size(ts::SMALL)
                                                .text_color(p.fg3)
                                                .hover(|s| s.text_color(p.acc))
                                                .on_click(cx.listener(move |_, _, _, cx| {
                                                    cx.stop_propagation();
                                                    cx.emit(FleetTabEvent::OpenTerminal(
                                                        term.clone(),
                                                    ));
                                                }))
                                                .child("Terminal"),
                                        ),
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1(),
            )
            .into_any_element()
    }

    /// The expanded Host's full output (or keys), virtualized.
    fn render_detail(&self, p: &Palette, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = self.expanded.clone()?;
        let ix = self.rows.rows.iter().position(|r| r.id == id)?;
        let r = &self.rows.rows[ix];
        let keys = self.mode == Mode::Keys;
        let count = if keys {
            r.key_list().len()
        } else {
            r.output.len()
        };
        let text = if keys {
            r.key_list()
                .iter()
                .map(|k| {
                    format!(
                        "{} {} {}",
                        k.fingerprint.as_deref().unwrap_or("?"),
                        k.key.kind,
                        k.key.comment
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            r.output.text()
        };
        let session_key = match &r.keys {
            Some(Ok(k)) => k.session_key.clone(),
            _ => None,
        };
        let p2 = *p;
        Some(
            div()
                .flex_none()
                .h(rpx(240.))
                .flex()
                .flex_col()
                .border_t_1()
                .border_color(p.bd)
                .child(
                    div()
                        .h(rpx(32.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(rpx(8.))
                        .px(rpx(12.))
                        .border_b_1()
                        .border_color(p.bd)
                        .text_size(ts::BODY)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(format!(
                                    "{} · {} {}{}",
                                    r.name,
                                    count,
                                    if keys { "keys" } else { "lines" },
                                    if r.output.truncated {
                                        " (truncated)"
                                    } else {
                                        ""
                                    }
                                )),
                        )
                        .child(
                            ui::button("fleet-copy-one", "Copy", Kind::Ghost, p)
                                .h(rpx(22.))
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))
                                }),
                        )
                        .child(
                            ui::button("fleet-close-detail", "Close", Kind::Ghost, p)
                                .h(rpx(22.))
                                .on_click(cx.listener(|t, _, _, cx| {
                                    t.expanded = None;
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    uniform_list(
                        "fleet-output",
                        count,
                        cx.processor(move |this, range: std::ops::Range<usize>, _w, _cx| {
                            let p = &p2;
                            let Some(r) = this.rows.rows.get(ix) else {
                                return Vec::new();
                            };
                            range
                                .map(|i| {
                                    let (err, text): (bool, String) = if keys {
                                        match r.key_list().get(i) {
                                            Some(k) => {
                                                let own = k.fingerprint.is_some()
                                                    && k.fingerprint == session_key;
                                                (
                                                    false,
                                                    format!(
                                                        "{:<4} {}  {}  {}{}{}",
                                                        k.line,
                                                        k.fingerprint
                                                            .as_deref()
                                                            .unwrap_or("(unreadable)"),
                                                        k.key.kind,
                                                        k.key.comment,
                                                        k.options
                                                            .as_deref()
                                                            .map(|o| format!("  [{o}]"))
                                                            .unwrap_or_default(),
                                                        if own { "  ← this session" } else { "" }
                                                    ),
                                                )
                                            }
                                            None => (false, String::new()),
                                        }
                                    } else {
                                        r.output
                                            .line(i)
                                            .map(|(e, l)| (e, l.to_owned()))
                                            .unwrap_or_default()
                                    };
                                    div()
                                        .h(rpx(18.))
                                        .px(rpx(12.))
                                        .font_family(MONO)
                                        .text_size(ts::SMALL)
                                        .whitespace_nowrap()
                                        .text_color(if err { p.prod } else { p.fg })
                                        .child(text)
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                    .flex_1(),
                )
                .into_any_element(),
        )
    }
}

impl Render for FleetTab {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let shown = if self.mode == Mode::Dashboard {
            model::dash_order(&self.rows.rows, self.sort.0, self.sort.1)
        } else {
            self.rows.visible(self.failed_only, self.differs_only)
        };
        let header = self.render_header(&p, cx);
        let targets = self.render_targets(&p, cx);
        let picker = self.picker_open.then(|| self.render_picker(&p, cx));
        let form = self.render_form(&p, cx);
        let confirm = self.render_confirm(&p, cx);
        let tools = (self.mode != Mode::Dashboard).then(|| self.render_tools(&shown, &p, cx));
        let grid = self.render_grid(shown, &p, cx);
        let detail = self.render_detail(&p, cx);
        div()
            .id("fleet-tab")
            .size_full()
            .flex()
            .flex_col()
            .font_family(SANS)
            .bg(p.bg)
            .text_color(p.fg)
            .child(header)
            .child(targets)
            .children(picker)
            .child(form)
            .children(confirm)
            .children(tools)
            .child(grid)
            .children(detail)
    }
}

impl Workspace {
    /// Open a Fleet tab doing `mode` on `pick` (focus an identical one).
    pub(crate) fn open_fleet(
        &mut self,
        mode: Mode,
        pick: Pick,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self.tabs.iter().position(|t| {
            matches!(t, Tab::Fleet(f) if { let f = f.read(cx); f.mode == mode && f.pick == pick })
        }) {
            self.activate(ix, cx);
            return;
        }
        let core = self.core.clone();
        let profiles = self.profiles.all.clone();
        let tab = cx.new(|cx| FleetTab::new(core, mode, pick, &profiles, window, cx));
        cx.subscribe(&tab, |this, _, ev: &FleetTabEvent, cx| match ev {
            FleetTabEvent::OpenTerminal(id) => this.open_terminal(Some(id.clone()), cx),
        })
        .detach();
        self.tabs.push(Tab::Fleet(tab));
        self.activate(self.tabs.len() - 1, cx);
    }

    /// The Hosts Fleet commands from the palette work on: the Explorer's selection,
    /// else every Host.
    pub(crate) fn fleet_palette_pick(&self) -> Pick {
        let hosts: Vec<ProfileId> = self
            .host_selection
            .iter()
            .filter(|id| self.profiles.host(id).is_some())
            .cloned()
            .collect();
        if hosts.is_empty() {
            Pick::All
        } else {
            Pick::Hosts(hosts)
        }
    }

    /// Route a Fleet event: the inventory preview opens the import dialog, the rest goes
    /// to the tabs.
    pub(crate) fn on_fleet_event(
        &mut self,
        ev: FleetEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let FleetEvent::InventoryPreview { path, result } = ev {
            match result {
                Ok(hosts) => {
                    let chosen = hosts
                        .iter()
                        .filter(|h| !h.exists)
                        .map(|h| h.alias.clone())
                        .collect();
                    self.overlay = Some(crate::overlays::Overlay::SshImport(
                        crate::overlays::SshImportPreview {
                            path: path.clone(),
                            hosts,
                            chosen,
                            inventory: Some(path),
                        },
                    ));
                    window.focus(&self.overlay_focus, cx);
                }
                Err(e) => self.toast(format!("Inventory: {e}"), cx),
            }
            cx.notify();
            return;
        }
        for t in &self.tabs {
            if let Tab::Fleet(f) = t {
                f.update(cx, |f, cx| f.on_event(&ev, cx));
            }
        }
    }

    /// Pick an Ansible inventory and preview its Hosts.
    pub(crate) fn import_ansible(&mut self, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import inventory".into()),
        });
        let core = self.core.clone();
        cx.spawn(async move |_, _| {
            if let Ok(Ok(Some(mut paths))) = chosen.await
                && let Some(path) = paths.pop()
            {
                core.send(Command::Fleet(FleetCommand::PreviewInventory { path }));
            }
        })
        .detach();
    }
}
