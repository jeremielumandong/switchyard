//! The Driver Manager card in the connection editor: what is missing and why, then
//! install (license, admin command, download progress, verify), use an existing path, or
//! manual steps; on success the connection test runs again.

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px, relative,
};
use switchyard_core::drivers::{Component, ComponentStatus, InstallPlan, InstallProgress};
use switchyard_core::{Command, Event};

use super::{ConnEditor, ConnEditorEvent, TestState};
use crate::appearance::{rpx, ts};
use crate::theme::{MONO, Palette, SANS};
use crate::ui::{self, Kind};

/// Where the card is in its flow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Step {
    /// Explain what is missing.
    Explain,
    /// Click-through license before download.
    License,
    /// Show the admin command and offer to run it.
    Confirm,
    /// Manual steps.
    Steps,
    /// Type a path.
    Path,
    /// Install running.
    Working(InstallProgress),
    /// Waiting for a re-check the user asked for.
    Rechecking,
    /// Install failed.
    Failed {
        message: String,
        command: Option<String>,
    },
    /// Ready; the test runs again.
    Installed(String),
}

/// Card state, kept by the editor.
pub(super) struct DriverCard {
    pub(super) id: String,
    pub(super) step: Step,
}

impl DriverCard {
    pub(super) fn new(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            step: Step::Explain,
        }
    }
}

/// The "Use existing path" input (made with the editor, which has a window).
pub(super) fn path_input(window: &mut Window, cx: &mut Context<ConnEditor>) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx).placeholder("Library file, or the folder that holds it")
    })
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_048_576.0)
}

impl ConnEditor {
    /// The component the card is about.
    fn card_component(&self) -> Option<&Component> {
        let id = &self.card.as_ref()?.id;
        self.components.iter().find(|c| &c.id == id)
    }

    fn set_step(&mut self, step: Step, cx: &mut Context<Self>) {
        if let Some(card) = self.card.as_mut() {
            card.step = step;
        }
        cx.notify();
    }

    fn start_install(&mut self, accept_license: bool, cx: &mut Context<Self>) {
        let Some(c) = self.card_component() else {
            return;
        };
        let first = match &c.plan {
            InstallPlan::Package { display, .. } => InstallProgress::Running {
                command: display.clone(),
            },
            InstallPlan::Archive { size, .. } => InstallProgress::Downloading {
                done: 0,
                total: Some(*size),
            },
            _ => InstallProgress::Verifying,
        };
        self.core.send(Command::InstallComponent {
            id: c.id.clone(),
            accept_license,
        });
        self.set_step(Step::Working(first), cx);
    }

    /// "Install automatically": the next step depends on the strategy.
    fn on_install_clicked(&mut self, cx: &mut Context<Self>) {
        let Some(c) = self.card_component() else {
            return;
        };
        let next = match &c.plan {
            InstallPlan::Archive { .. }
                if c.license.as_ref().is_some_and(|l| l.accept_required) =>
            {
                Step::License
            }
            InstallPlan::Package { .. } => Step::Confirm,
            InstallPlan::Manual { .. } => Step::Steps,
            InstallPlan::Unavailable { reason } => Step::Failed {
                message: reason.clone(),
                command: None,
            },
            InstallPlan::Archive { .. } | InstallPlan::Builtin => {
                self.start_install(false, cx);
                return;
            }
        };
        self.set_step(next, cx);
    }

    /// Driver Manager events while the editor is open.
    pub(crate) fn on_component_event(&mut self, ev: &Event, cx: &mut Context<Self>) {
        let Some(card) = self.card.as_ref() else {
            if let Event::Components(list) = ev {
                self.components = list.clone();
            }
            return;
        };
        let id = card.id.clone();
        match ev {
            Event::Components(list) => {
                self.components = list.clone();
                if card.step == Step::Rechecking {
                    match self.card_component().map(|c| c.status.clone()) {
                        Some(ComponentStatus::Installed { location, .. }) => {
                            self.component_ready(location, cx)
                        }
                        _ => self.set_step(
                            Step::Failed {
                                message: "Still not found. Check the command finished, or use \
                                          an existing path."
                                    .into(),
                                command: None,
                            },
                            cx,
                        ),
                    }
                }
            }
            Event::ComponentProgress { id: i, progress } if *i == id => {
                self.set_step(Step::Working(progress.clone()), cx)
            }
            Event::ComponentInstalled { component } if component.id == id => {
                let location = match &component.status {
                    ComponentStatus::Installed { location, .. } => location.clone(),
                    _ => String::new(),
                };
                self.component_ready(location, cx);
            }
            Event::ComponentFailed {
                id: i,
                message,
                command,
            } if *i == id => self.set_step(
                Step::Failed {
                    message: message.clone(),
                    command: command.clone(),
                },
                cx,
            ),
            _ => {}
        }
    }

    /// Registered: retry the original connection automatically.
    fn component_ready(&mut self, location: String, cx: &mut Context<Self>) {
        self.set_step(Step::Installed(location), cx);
        if let Some(c) = self.card_component() {
            cx.emit(ConnEditorEvent::Toast(format!("{} ready", c.name)));
        }
        self.test = TestState::Idle;
        self.test(cx);
    }

    pub(super) fn render_driver_card(&self, p: &Palette, cx: &mut Context<Self>) -> AnyElement {
        let (Some(card), Some(c)) = (self.card.as_ref(), self.card_component()) else {
            return div().into_any_element();
        };
        let warn = (p.stg, p.stg_bg);
        let bad = (p.prod, p.prod_bg);
        let good = (p.dev, p.dev_bg);
        let info = (p.acc, p.sel);
        let plan_meta = match &c.plan {
            InstallPlan::Package {
                manager,
                package,
                size,
                ..
            } => format!(
                "{}{} package {package}",
                size.as_ref().map(|s| format!("{s} · ")).unwrap_or_default(),
                manager
            ),
            InstallPlan::Archive { size, url, .. } => {
                let host = url.split('/').nth(2).unwrap_or(url);
                format!("{} · download from {host}", mb(*size))
            }
            InstallPlan::Manual { .. } => "Set up by hand".into(),
            InstallPlan::Builtin => "Built into the OS".into(),
            InstallPlan::Unavailable { reason } => reason.clone(),
        };
        let license = c
            .license
            .as_ref()
            .map(|l| format!("{} license", l.name))
            .unwrap_or_default();
        let meta = [plan_meta, license]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");

        let (tag, (fg, bg), title, body): (&str, _, String, String) = match &card.step {
            Step::Explain | Step::Path => match &c.status {
                ComponentStatus::TooOld {
                    version, required, ..
                } => (
                    "OUTDATED",
                    warn,
                    format!("{} {version} is too old", c.name),
                    format!("{} needs {required} or newer.", c.needed_for),
                ),
                ComponentStatus::TooNew {
                    version,
                    supported_below,
                    ..
                } => (
                    "UNTESTED",
                    warn,
                    format!("{} {version} is newer than Switchyard supports", c.name),
                    format!(
                        "Switchyard was tested with versions below {supported_below}; its options may have changed."
                    ),
                ),
                _ => (
                    "MISSING",
                    warn,
                    format!("{} is not installed", c.name),
                    format!("{} needs it, and Switchyard did not find it.", c.needed_for),
                ),
            },
            Step::License => (
                "LICENSE",
                info,
                format!(
                    "Accept the {} license",
                    c.license.as_ref().map_or("vendor", |l| l.name.as_str())
                ),
                format!(
                    "{} is distributed under its vendor's terms. Read them before downloading.",
                    c.name
                ),
            ),
            Step::Confirm => (
                "NEEDS ADMIN",
                warn,
                "This one installs as a system package".into(),
                "Switchyard runs this command; your system asks for your password:".into(),
            ),
            Step::Steps => (
                "MANUAL",
                info,
                format!("Set up {} by hand", c.name),
                "Follow these steps, then re-check:".into(),
            ),
            Step::Working(InstallProgress::Downloading { .. }) => (
                "DOWNLOADING",
                info,
                format!("Downloading {}", c.name),
                "Into Switchyard's own folder; no admin rights needed.".into(),
            ),
            Step::Working(InstallProgress::Verifying) => (
                "VERIFYING",
                info,
                "Checking the download".into(),
                "SHA-256 against the signed manifest.".into(),
            ),
            Step::Working(InstallProgress::Unpacking) => (
                "INSTALLING",
                info,
                format!("Unpacking {}", c.name),
                "Verified; unpacking into the app-managed directory.".into(),
            ),
            Step::Working(InstallProgress::Running { .. }) => (
                "INSTALLING",
                info,
                format!("Installing {}", c.name),
                "Approve the system's password prompt to continue.".into(),
            ),
            Step::Rechecking => (
                "CHECKING",
                info,
                format!("Looking for {}", c.name),
                String::new(),
            ),
            Step::Failed {
                command: Some(_), ..
            } => (
                "NEEDS ADMIN",
                warn,
                "Run this in a terminal".into(),
                "Switchyard cannot ask for admin rights here. Run the command, then re-check:"
                    .into(),
            ),
            Step::Failed { message, .. } => (
                "FAILED",
                bad,
                format!("Could not set up {}", c.name),
                message.clone(),
            ),
            Step::Installed(location) => (
                "INSTALLED",
                good,
                format!("{} is ready", c.name),
                format!("Found at {location}. Retrying the connection…"),
            ),
        };

        let command_box = |cmd: String, id: &'static str| {
            let copy = cmd.clone();
            div()
                .mx(rpx(14.))
                .mb(rpx(12.))
                .flex()
                .items_center()
                .gap(rpx(8.))
                .px(rpx(10.))
                .py(rpx(7.))
                .border_1()
                .border_color(p.bd)
                .rounded(px(6.))
                .bg(p.surface)
                .font_family(MONO)
                .text_size(ts::BODY)
                .child(div().text_color(p.fg3).child("$"))
                .child(div().flex_1().child(cmd))
                .child(
                    div()
                        .id(id)
                        .font_family(SANS)
                        .text_size(ts::LABEL)
                        .text_color(p.acc)
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                            cx.emit(ConnEditorEvent::Toast("Command copied".into()));
                        }))
                        .child("Copy"),
                )
        };

        let extra: Option<AnyElement> =
            match &card.step {
                Step::Confirm
                | Step::Failed {
                    command: Some(_), ..
                } => {
                    let cmd = match (&card.step, &c.plan) {
                        (
                            Step::Failed {
                                command: Some(cmd), ..
                            },
                            _,
                        ) => cmd.clone(),
                        (_, InstallPlan::Package { display, .. }) => display.clone(),
                        _ => String::new(),
                    };
                    Some(command_box(cmd, "drv-copy").into_any_element())
                }
                Step::Steps => {
                    let steps = match &c.plan {
                        InstallPlan::Manual { steps } => steps.clone(),
                        InstallPlan::Package { display, .. } => vec![display.clone()],
                        _ => Vec::new(),
                    };
                    Some(
                        div()
                            .mx(rpx(14.))
                            .mb(rpx(12.))
                            .flex()
                            .flex_col()
                            .gap(rpx(4.))
                            .text_size(ts::BODY)
                            .children(steps.into_iter().enumerate().map(|(i, s)| {
                                div()
                                    .flex()
                                    .gap(rpx(8.))
                                    .child(div().text_color(p.fg3).child(format!("{}.", i + 1)))
                                    .child(div().flex_1().min_w_0().font_family(MONO).child(s))
                            }))
                            .into_any_element(),
                    )
                }
                Step::License => c.license.as_ref().and_then(|l| l.url.clone()).map(|url| {
                    let open = url.clone();
                    div()
                        .mx(rpx(14.))
                        .mb(rpx(12.))
                        .id("drv-license")
                        .text_size(ts::BODY)
                        .text_color(p.acc)
                        .on_click(move |_, _, cx| cx.open_url(&open))
                        .child(format!("Read the license: {url}"))
                        .into_any_element()
                }),
                Step::Path => Some(
                    div()
                        .mx(rpx(14.))
                        .mb(rpx(12.))
                        .h(rpx(28.))
                        .flex()
                        .items_center()
                        .px(rpx(9.))
                        .border_1()
                        .border_color(p.bd2)
                        .rounded(px(6.))
                        .bg(p.surface)
                        .child(
                            Input::new(&self.driver_path)
                                .appearance(false)
                                .text_size(ts::BODY),
                        )
                        .into_any_element(),
                ),
                Step::Working(progress) => {
                    let (frac, label) = match progress {
                        InstallProgress::Downloading { done, total } => match total {
                            Some(t) if *t > 0 => (
                                (*done as f32 / *t as f32).min(1.0),
                                format!("{} of {}", mb(*done), mb(*t)),
                            ),
                            _ => (0.1, mb(*done)),
                        },
                        InstallProgress::Verifying => (1.0, "Verifying SHA-256…".into()),
                        InstallProgress::Unpacking => (1.0, "Unpacking…".into()),
                        InstallProgress::Running { command } => (0.5, command.clone()),
                    };
                    Some(
                        div()
                            .mx(rpx(14.))
                            .mb(rpx(12.))
                            .flex()
                            .flex_col()
                            .gap(rpx(6.))
                            .child(
                                div().h(rpx(4.)).rounded(px(2.)).bg(p.bd).child(
                                    div().h_full().rounded(px(2.)).bg(p.acc).w(relative(frac)),
                                ),
                            )
                            .child(
                                div()
                                    .font_family(MONO)
                                    .text_size(ts::SMALL)
                                    .text_color(p.fg3)
                                    .child(label),
                            )
                            .into_any_element(),
                    )
                }
                _ => None,
            };

        let btn = |id: &'static str, label: &'static str, kind: Kind| {
            ui::button(SharedString::from(id), label, kind, p)
        };
        let actions: Vec<AnyElement> = match &card.step {
            Step::Explain => vec![
                btn("drv-install", "Install automatically", Kind::Primary)
                    .on_click(cx.listener(|this, _, _, cx| this.on_install_clicked(cx)))
                    .into_any_element(),
                btn("drv-path", "Use existing path…", Kind::Secondary)
                    .on_click(cx.listener(|this, _, w, cx| {
                        this.set_step(Step::Path, cx);
                        this.driver_path.update(cx, |i, cx| i.focus(w, cx));
                    }))
                    .into_any_element(),
                btn("drv-manual", "Show manual steps", Kind::Ghost)
                    .on_click(cx.listener(|this, _, _, cx| this.set_step(Step::Steps, cx)))
                    .into_any_element(),
            ],
            Step::License => vec![
                btn("drv-accept", "Accept and install", Kind::Primary)
                    .on_click(cx.listener(|this, _, _, cx| this.start_install(true, cx)))
                    .into_any_element(),
                btn("drv-cancel", "Cancel", Kind::Ghost)
                    .on_click(cx.listener(|this, _, _, cx| this.set_step(Step::Explain, cx)))
                    .into_any_element(),
            ],
            Step::Confirm => vec![
                btn("drv-run", "Install", Kind::Primary)
                    .on_click(cx.listener(|this, _, _, cx| this.start_install(false, cx)))
                    .into_any_element(),
                btn("drv-recheck", "I ran it myself — re-check", Kind::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.recheck(cx)))
                    .into_any_element(),
                btn("drv-cancel", "Cancel", Kind::Ghost)
                    .on_click(cx.listener(|this, _, _, cx| this.set_step(Step::Explain, cx)))
                    .into_any_element(),
            ],
            Step::Steps
            | Step::Failed {
                command: Some(_), ..
            } => vec![
                btn("drv-recheck", "Re-check", Kind::Primary)
                    .on_click(cx.listener(|this, _, _, cx| this.recheck(cx)))
                    .into_any_element(),
                btn("drv-cancel", "Back", Kind::Ghost)
                    .on_click(cx.listener(|this, _, _, cx| this.set_step(Step::Explain, cx)))
                    .into_any_element(),
            ],
            Step::Path => vec![
                btn("drv-use", "Use this path", Kind::Primary)
                    .on_click(cx.listener(|this, _, _, cx| {
                        let Some(card) = &this.card else { return };
                        let path = this.driver_path.read(cx).value().trim().to_owned();
                        if path.is_empty() {
                            return;
                        }
                        this.core.send(Command::UseComponentPath {
                            id: card.id.clone(),
                            path: path.into(),
                        });
                        this.set_step(Step::Rechecking, cx);
                    }))
                    .into_any_element(),
                btn("drv-cancel", "Cancel", Kind::Ghost)
                    .on_click(cx.listener(|this, _, _, cx| this.set_step(Step::Explain, cx)))
                    .into_any_element(),
            ],
            Step::Failed { .. } => vec![
                btn("drv-retry", "Retry", Kind::Primary)
                    .on_click(cx.listener(|this, _, _, cx| this.on_install_clicked(cx)))
                    .into_any_element(),
                btn("drv-path", "Use existing path…", Kind::Secondary)
                    .on_click(cx.listener(|this, _, _, cx| this.set_step(Step::Path, cx)))
                    .into_any_element(),
                btn("drv-cancel", "Back", Kind::Ghost)
                    .on_click(cx.listener(|this, _, _, cx| this.set_step(Step::Explain, cx)))
                    .into_any_element(),
            ],
            Step::Working(_) | Step::Rechecking | Step::Installed(_) => Vec::new(),
        };

        div()
            // Inside the scrolling form: keep full height instead of shrinking.
            .flex_none()
            .border_1()
            .border_color(fg)
            .rounded(px(8.))
            .bg(p.bg)
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .gap(rpx(12.))
                    .items_start()
                    .px(rpx(14.))
                    .py(rpx(12.))
                    .child(
                        div()
                            .flex_none()
                            .px(rpx(6.))
                            .rounded(px(4.))
                            .bg(bg)
                            .text_color(fg)
                            .font_family(MONO)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(ts::TINY)
                            .line_height(rpx(18.))
                            .child(tag),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap(rpx(4.))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(ts::BASE)
                                    .child(title),
                            )
                            .when(!body.is_empty(), |d| {
                                d.child(div().text_size(ts::BODY).text_color(p.fg2).child(body))
                            })
                            .child(
                                div()
                                    .font_family(MONO)
                                    .text_size(ts::SMALL)
                                    .text_color(p.fg3)
                                    .child(meta),
                            ),
                    ),
            )
            .children(extra)
            .when(!actions.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .gap(rpx(6.))
                        .px(rpx(14.))
                        .py(rpx(10.))
                        .border_t_1()
                        .border_color(p.bd)
                        .bg(p.panel)
                        .children(actions),
                )
            })
            .into_any_element()
    }

    fn recheck(&mut self, cx: &mut Context<Self>) {
        self.core.send(Command::DetectComponents);
        self.set_step(Step::Rechecking, cx);
    }
}
