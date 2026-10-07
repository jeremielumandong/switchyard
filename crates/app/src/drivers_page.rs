//! Settings → Drivers: built-in drivers, optional native components with their status,
//! install / remove / steps, install from file, and the download mirror.

use std::collections::HashMap;

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FontWeight, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use switchyard_core::drivers::{
    BUILTIN_DRIVERS, Component, ComponentStatus, InstallPlan, InstallProgress, Source,
};
use switchyard_core::{Command, Event};

use crate::overlays::Overlay;
use crate::theme::{MONO, Palette};
use crate::ui::{self, Kind};
use crate::workspace::Workspace;

/// An install the page is following.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriverJob {
    /// Under way.
    Working(InstallProgress),
    /// Failed; `command` when it must be run in a terminal.
    Failed {
        /// Why.
        message: String,
        /// Command to run by hand.
        command: Option<String>,
    },
}

/// Page state that outlives a render.
#[derive(Default)]
pub struct DriversPage {
    jobs: HashMap<String, DriverJob>,
    /// Component waiting for license acceptance.
    license: Option<String>,
    /// Component whose manual steps are expanded.
    steps: Option<String>,
    file: Option<Entity<InputState>>,
    mirror: Option<Entity<InputState>>,
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

impl Workspace {
    /// Driver Manager events: the page's jobs and an open connection editor's card.
    pub(crate) fn on_component_event(&mut self, ev: Event, cx: &mut Context<Self>) {
        match &ev {
            Event::Components(c) => {
                // Anything found since (installed by hand, re-checked) ends its job.
                for comp in c {
                    if comp.status.is_installed()
                        && matches!(
                            self.drivers.jobs.get(&comp.id),
                            Some(DriverJob::Failed { .. })
                        )
                    {
                        self.drivers.jobs.remove(&comp.id);
                    }
                }
                self.components = c.clone();
                if let Some(v) = &self.assistant_view {
                    v.update(cx, |v, cx| {
                        v.set_components(c);
                        cx.notify();
                    });
                }
            }
            Event::ComponentProgress { id, progress } => {
                self.drivers
                    .jobs
                    .insert(id.clone(), DriverJob::Working(progress.clone()));
            }
            Event::ComponentInstalled { component } => {
                self.drivers.jobs.remove(&component.id);
                if let Some(c) = self.components.iter_mut().find(|c| c.id == component.id) {
                    *c = component.clone();
                }
                self.toast(format!("{} is ready", component.name), cx);
            }
            Event::ComponentFailed {
                id,
                message,
                command,
            } => {
                self.drivers.jobs.insert(
                    id.clone(),
                    DriverJob::Failed {
                        message: message.clone(),
                        command: command.clone(),
                    },
                );
            }
            _ => return,
        }
        if let Some(Overlay::ConnEditor(ed)) = &self.overlay {
            ed.update(cx, |ed, cx| ed.on_component_event(&ev, cx));
        }
        cx.notify();
    }

    fn install_component(&mut self, id: &str, accept_license: bool, cx: &mut Context<Self>) {
        self.drivers.license = None;
        self.drivers.jobs.insert(
            id.to_owned(),
            DriverJob::Working(InstallProgress::Downloading {
                done: 0,
                total: None,
            }),
        );
        self.core.send(Command::InstallComponent {
            id: id.to_owned(),
            accept_license,
        });
        cx.notify();
    }

    fn on_row_action(&mut self, c: &Component, cx: &mut Context<Self>) {
        let id = c.id.clone();
        match (&c.status, &c.plan) {
            (
                ComponentStatus::Installed {
                    source: Source::AppManaged | Source::UserPath,
                    ..
                },
                _,
            ) => self.core.send(Command::RemoveComponent { id }),
            (ComponentStatus::Installed { .. }, _) => {}
            (_, InstallPlan::Manual { .. }) => {
                self.drivers.steps = (self.drivers.steps.as_deref() != Some(&id)).then_some(id);
            }
            (_, InstallPlan::Archive { .. })
                if c.license.as_ref().is_some_and(|l| l.accept_required) =>
            {
                self.drivers.license = Some(id);
            }
            _ => {
                if let InstallPlan::Package { display, .. } = &c.plan {
                    self.drivers.jobs.insert(
                        id.clone(),
                        DriverJob::Working(InstallProgress::Running {
                            command: display.clone(),
                        }),
                    );
                    self.core.send(Command::InstallComponent {
                        id,
                        accept_license: false,
                    });
                } else {
                    self.install_component(&id, false, cx);
                    return;
                }
            }
        }
        cx.notify();
    }

    pub(crate) fn render_drivers(
        &mut self,
        p: &Palette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let file = self
            .drivers
            .file
            .get_or_insert_with(|| {
                cx.new(|cx| InputState::new(window, cx).placeholder("Path to a downloaded archive"))
            })
            .clone();
        let mirror = self
            .drivers
            .mirror
            .get_or_insert_with(|| {
                cx.new(|cx| {
                    InputState::new(window, cx).placeholder("https://mirror.example.com/switchyard")
                })
            })
            .clone();
        let grid_row = |cells: Vec<AnyElement>| {
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .min_h(px(46.))
                .px(px(12.))
                .py(px(6.))
                .text_size(px(12.5))
                .children(cells)
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        for c in &self.components {
            let job = self.drivers.jobs.get(&c.id);
            let (status, color): (String, _) = match (job, &c.status) {
                (Some(DriverJob::Working(InstallProgress::Downloading { done, total })), _) => (
                    match total {
                        Some(t) if *t > 0 => format!("Downloading {}%", done * 100 / t),
                        _ => format!("Downloading {}", mb(*done)),
                    },
                    p.acc,
                ),
                (Some(DriverJob::Working(InstallProgress::Verifying)), _) => {
                    ("Verifying".into(), p.acc)
                }
                (Some(DriverJob::Working(InstallProgress::Unpacking)), _) => {
                    ("Unpacking".into(), p.acc)
                }
                (Some(DriverJob::Working(InstallProgress::Running { .. })), _) => {
                    ("Installing".into(), p.acc)
                }
                (
                    Some(DriverJob::Failed {
                        command: Some(_), ..
                    }),
                    _,
                ) => ("Needs admin".into(), p.stg),
                (Some(DriverJob::Failed { .. }), _) => ("Failed".into(), p.prod),
                (None, ComponentStatus::Installed { source, .. }) => (
                    match source {
                        Source::AppManaged => "Installed",
                        Source::UserPath => "Using your path",
                        Source::Builtin => "Built in",
                        Source::Environment | Source::System => "Detected",
                    }
                    .into(),
                    p.dev,
                ),
                (None, ComponentStatus::TooOld { .. }) => ("Too old".into(), p.stg),
                (None, ComponentStatus::TooNew { .. }) => ("Untested version".into(), p.stg),
                (None, ComponentStatus::Missing) => ("Not installed".into(), p.fg3),
            };
            let (location, version) = match &c.status {
                ComponentStatus::Installed {
                    location, version, ..
                } => (location.clone(), version.clone().unwrap_or("—".into())),
                ComponentStatus::TooOld {
                    location, version, ..
                }
                | ComponentStatus::TooNew {
                    location, version, ..
                } => (location.clone(), version.clone()),
                ComponentStatus::Missing => ("—".into(), "—".into()),
            };
            let working = matches!(job, Some(DriverJob::Working(_)));
            let action: Option<&'static str> = match (&c.status, &c.plan) {
                _ if working => None,
                (
                    ComponentStatus::Installed {
                        source: Source::AppManaged | Source::UserPath,
                        ..
                    },
                    _,
                ) => Some("Remove"),
                (ComponentStatus::Installed { .. }, _) => None,
                (_, InstallPlan::Manual { .. }) => Some("Show steps"),
                (_, InstallPlan::Unavailable { .. } | InstallPlan::Builtin) => None,
                (_, _) if matches!(job, Some(DriverJob::Failed { .. })) => Some("Retry"),
                _ => Some("Install"),
            };
            let row_c = c.clone();
            rows.push(
                grid_row(vec![
                    div()
                        .w(px(180.))
                        .flex_none()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().font_weight(FontWeight::MEDIUM).child(c.name.clone()))
                        .child(
                            div()
                                .font_family(MONO)
                                .text_size(px(10.5))
                                .text_color(p.fg3)
                                .truncate()
                                .child(location),
                        )
                        .into_any_element(),
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.))
                        .text_color(p.fg2)
                        .child(c.needed_for.clone())
                        .into_any_element(),
                    div()
                        .w(px(90.))
                        .font_family(MONO)
                        .text_size(px(11.5))
                        .text_color(p.fg2)
                        .child(version)
                        .into_any_element(),
                    div()
                        .w(px(130.))
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .text_size(px(12.))
                        .text_color(color)
                        .child(ui::dot(color, 6.))
                        .child(status)
                        .into_any_element(),
                    div()
                        .w(px(96.))
                        .flex()
                        .justify_end()
                        .when_some(action, |d, label| {
                            d.child(
                                ui::button(
                                    SharedString::from(format!("drv-{}", c.id)),
                                    label,
                                    Kind::Secondary,
                                    p,
                                )
                                .h(px(24.))
                                .text_size(px(11.5))
                                .on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.on_row_action(&row_c, cx)
                                    }),
                                ),
                            )
                        })
                        .into_any_element(),
                ])
                .into_any_element(),
            );
            // Details under the row: license, steps, command, error.
            let detail = |child: AnyElement| {
                div()
                    .px(px(12.))
                    .pb(px(10.))
                    .text_size(px(12.))
                    .text_color(p.fg2)
                    .child(child)
                    .into_any_element()
            };
            if self.drivers.license.as_deref() == Some(c.id.as_str()) {
                let l = c.license.clone().unwrap_or_default();
                let id = c.id.clone();
                rows.push(detail(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(div().flex_1().min_w_0().child(format!(
                            "{} is under the {}{}. Downloading means you accept it.",
                            c.name,
                            l.name,
                            l.url.map(|u| format!(" ({u})")).unwrap_or_default()
                        )))
                        .child(
                            ui::button("drv-accept", "Accept and install", Kind::Primary, p)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.install_component(&id, true, cx)
                                })),
                        )
                        .child(
                            ui::button("drv-decline", "Cancel", Kind::Ghost, p).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.drivers.license = None;
                                    cx.notify();
                                }),
                            ),
                        )
                        .into_any_element(),
                ));
            }
            if self.drivers.steps.as_deref() == Some(c.id.as_str())
                && let InstallPlan::Manual { steps } = &c.plan
            {
                rows.push(detail(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(3.))
                        .children(steps.iter().enumerate().map(|(i, s)| {
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(div().text_color(p.fg3).child(format!("{}.", i + 1)))
                                .child(div().flex_1().min_w_0().font_family(MONO).child(s.clone()))
                        }))
                        .into_any_element(),
                ));
            }
            match job {
                Some(DriverJob::Working(InstallProgress::Running { command })) => {
                    rows.push(detail(
                        div()
                            .font_family(MONO)
                            .child(format!("$ {command}  · approve the password prompt"))
                            .into_any_element(),
                    ));
                }
                Some(DriverJob::Failed { message, command }) => {
                    rows.push(detail(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .child(
                                div()
                                    .text_color(if command.is_some() { p.fg2 } else { p.prod })
                                    .child(if command.is_some() {
                                        "Run this in a terminal, then Check again:".to_owned()
                                    } else {
                                        message.clone()
                                    }),
                            )
                            .when_some(command.clone(), |d, cmd| {
                                d.child(div().font_family(MONO).child(format!("$ {cmd}")))
                            })
                            .into_any_element(),
                    ));
                }
                _ => {}
            }
            rows.push(div().h(px(1.)).bg(p.line).into_any_element());
        }

        let file_target = self
            .components
            .iter()
            .find(|c| matches!(c.plan, InstallPlan::Archive { .. }))
            .map(|c| (c.id.clone(), c.name.clone()));
        let file_label: SharedString = match &file_target {
            Some((_, name)) => format!("Install {name} from file").into(),
            None => "Install from file".into(),
        };

        div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .p(px(18.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(div().text_size(px(12.5)).text_color(p.fg2).child("Built-in drivers need nothing installed. Optional native components are loaded at runtime — a missing one disables a single feature, never the app."))
                    .child(div().flex().flex_wrap().gap(px(6.)).children(BUILTIN_DRIVERS.iter().map(|d| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .h(px(24.))
                            .px(px(9.))
                            .border_1()
                            .border_color(p.bd)
                            .rounded(px(12.))
                            .text_size(px(11.5))
                            .text_color(p.fg2)
                            .child(ui::dot(p.dev, 6.))
                            .child(format!("{} · {}", d.protocol, d.implementation))
                    }))),
            )
            .child(
                div()
                    .border_1()
                    .border_color(p.bd)
                    .rounded(px(8.))
                    .overflow_hidden()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .h(px(30.))
                            .px(px(12.))
                            .bg(p.panel)
                            .text_size(px(11.5))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.fg2)
                            .border_b_1()
                            .border_color(p.bd)
                            .child(div().w(px(180.)).flex_none().child("Component"))
                            .child(div().flex_1().min_w_0().child("Needed for"))
                            .child(div().w(px(90.)).child("Version"))
                            .child(div().w(px(130.)).child("Status"))
                            .child(div().w(px(96.))),
                    )
                    .children(rows),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(field(&file, p))
                    .child(
                        ui::button("drv-file", file_label, Kind::Secondary, p)
                            .when(file_target.is_none(), |b| b.opacity(0.5))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let Some((id, _)) = file_target.clone() else {
                                    this.toast("No component in this build installs from a file", cx);
                                    return;
                                };
                                let path = this
                                    .drivers
                                    .file
                                    .as_ref()
                                    .map(|f| f.read(cx).value().trim().to_owned())
                                    .unwrap_or_default();
                                if path.is_empty() {
                                    return;
                                }
                                this.drivers.jobs.insert(id.clone(), DriverJob::Working(InstallProgress::Verifying));
                                this.core.send(Command::InstallComponentFromFile { id, path: path.into() });
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(field(&mirror, p))
                    .child(ui::button("drv-mirror", "Use mirror", Kind::Secondary, p).on_click(
                        cx.listener(|this, _, _, cx| {
                            let url = this
                                .drivers
                                .mirror
                                .as_ref()
                                .map(|f| f.read(cx).value().trim().to_owned())
                                .filter(|u| !u.is_empty());
                            let msg = if url.is_some() { "Downloads now use the mirror" } else { "Downloads use vendor URLs" };
                            this.core.send(Command::SetDriverMirror { url });
                            this.toast(msg, cx);
                        }),
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(12.))
                    .text_color(p.fg2)
                    .child(ui::button("drv-check", "Check again", Kind::Secondary, p).on_click(cx.listener(|this, _, _, cx| {
                        this.drivers.jobs.retain(|_, j| matches!(j, DriverJob::Working(_)));
                        this.core.send(Command::DetectComponents);
                        this.toast("Checked components", cx);
                    })))
                    .child(div().flex_1())
                    .child("Archives are checked against a signed manifest (minisign + SHA-256)"),
            )
            .into_any_element()
    }
}

fn field(input: &Entity<InputState>, p: &Palette) -> impl IntoElement {
    div()
        .flex_1()
        .h(px(28.))
        .flex()
        .items_center()
        .px(px(9.))
        .border_1()
        .border_color(p.bd2)
        .rounded(px(6.))
        .bg(p.bg)
        .child(Input::new(input).appearance(false).text_size(px(12.)))
}
