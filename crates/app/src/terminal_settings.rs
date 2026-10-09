//! Settings → Terminal: session logging. The saved settings live in a global that
//! terminal tabs read.

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, Global,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use switchyard_core::term_settings::{
    DEFAULT_LOG_TEMPLATE, LogFormat, TERMINAL_SETTINGS_KEY, TerminalSettings,
};
use switchyard_core::{Command, RuntimeHandle};

use crate::theme::{MONO, Palette, palette};
use crate::ui::{self, Kind};

/// The saved terminal settings.
#[derive(Default)]
struct TermPrefs(TerminalSettings);

impl Global for TermPrefs {}

/// The terminal settings in effect.
pub fn settings(cx: &App) -> TerminalSettings {
    cx.try_global::<TermPrefs>()
        .map(|p| p.0.clone())
        .unwrap_or_default()
}

/// Replace the settings in effect (loaded or saved).
pub fn apply(s: TerminalSettings, cx: &mut App) {
    cx.set_global(TermPrefs(s));
}

/// The settings page.
pub struct TerminalSettingsView {
    core: RuntimeHandle,
    auto_log: bool,
    format: LogFormat,
    timestamps: bool,
    folder: Entity<InputState>,
    template: Entity<InputState>,
    saved: bool,
}

fn input(
    window: &mut Window,
    cx: &mut Context<TerminalSettingsView>,
    value: &str,
    placeholder: &str,
) -> Entity<InputState> {
    let (v, ph) = (value.to_owned(), placeholder.to_owned());
    cx.new(|cx| InputState::new(window, cx).placeholder(ph).default_value(v))
}

impl TerminalSettingsView {
    /// A view of the settings in effect.
    pub fn new(core: RuntimeHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let s = settings(cx);
        Self {
            core,
            auto_log: s.log.auto,
            format: s.log.format,
            timestamps: s.log.timestamps,
            folder: input(
                window,
                cx,
                s.log.folder.as_deref().unwrap_or(""),
                "Default: the app's data folder / terminal-logs",
            ),
            template: input(window, cx, &s.log.template, DEFAULT_LOG_TEMPLATE),
            saved: false,
        }
    }

    fn settings(&self, cx: &App) -> TerminalSettings {
        let mut s = settings(cx);
        let folder = self.folder.read(cx).value().trim().to_owned();
        let template = self.template.read(cx).value().trim().to_owned();
        s.log.auto = self.auto_log;
        s.log.format = self.format;
        s.log.timestamps = self.timestamps;
        s.log.folder = (!folder.is_empty()).then_some(folder);
        s.log.template = if template.is_empty() {
            DEFAULT_LOG_TEMPLATE.to_owned()
        } else {
            template
        };
        s
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let s = self.settings(cx);
        match serde_json::to_value(&s) {
            Ok(value) => {
                self.core.send(Command::SetSetting {
                    key: TERMINAL_SETTINGS_KEY.into(),
                    value,
                });
                apply(s, cx);
                self.saved = true;
            }
            Err(e) => tracing::warn!(error = %e, "terminal settings not saved"),
        }
        cx.notify();
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        self.saved = false;
        cx.notify();
    }
}

pub(crate) fn field(
    label: &str,
    e: &Entity<InputState>,
    width: f32,
    p: &Palette,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .w(px(width))
        .child(
            div()
                .text_size(px(11.))
                .text_color(p.fg3)
                .child(label.to_owned()),
        )
        .child(
            div()
                .h(px(28.))
                .flex()
                .items_center()
                .px(px(8.))
                .border_1()
                .border_color(p.bd2)
                .rounded(px(6.))
                .bg(p.bg)
                .font_family(MONO)
                .text_size(px(12.))
                .child(Input::new(e).appearance(false).text_size(px(12.))),
        )
}

pub(crate) fn heading(text: &str, p: &Palette) -> impl IntoElement {
    div()
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(p.fg2)
        .child(text.to_owned())
}

fn hint(text: &str, p: &Palette) -> impl IntoElement {
    div()
        .text_size(px(11.))
        .text_color(p.fg3)
        .child(text.to_owned())
}

/// A row of mutually exclusive choices.
fn choice<T: Copy + PartialEq + 'static>(
    id: &'static str,
    options: &[(T, &'static str)],
    current: T,
    p: &Palette,
    cx: &mut Context<TerminalSettingsView>,
    set: fn(&mut TerminalSettingsView, T),
) -> impl IntoElement {
    div()
        .flex()
        .gap(px(6.))
        .children(options.iter().enumerate().map(|(i, &(value, label))| {
            let active = value == current;
            div()
                .id(SharedString::from(format!("{id}-{i}")))
                .px(px(10.))
                .py(px(5.))
                .border_1()
                .border_color(if active { p.acc } else { p.bd2 })
                .rounded(px(6.))
                .bg(if active { p.sel } else { p.bg })
                .text_size(px(12.))
                .on_click(cx.listener(move |this, _, _, cx| {
                    set(this, value);
                    this.changed(cx);
                }))
                .child(label)
        }))
}

impl Render for TerminalSettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let logging = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(heading("Session logs", &p))
            .child(hint(
                "Copy each terminal's output to a file. Start or stop it per tab with the Log button.",
                &p,
            ))
            .child(
                ui::checkbox(
                    "ts-auto-log",
                    self.auto_log,
                    "Log every terminal session automatically",
                    &p,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.auto_log = !this.auto_log;
                    this.changed(cx);
                })),
            )
            .child(choice(
                "ts-format",
                &[
                    (LogFormat::Plain, LogFormat::Plain.label()),
                    (LogFormat::Raw, LogFormat::Raw.label()),
                ],
                self.format,
                &p,
                cx,
                |this, v| this.format = v,
            ))
            .when(self.format == LogFormat::Plain, |d| {
                d.child(
                    ui::checkbox(
                        "ts-stamps",
                        self.timestamps,
                        "Timestamp each line",
                        &p,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.timestamps = !this.timestamps;
                        this.changed(cx);
                    })),
                )
            })
            .child(
                div()
                    .flex()
                    .gap(px(10.))
                    .child(field("Folder", &self.folder, 300., &p))
                    .child(field("File name", &self.template, 220., &p)),
            )
            .child(hint(
                "File name placeholders: {host} {date} {time} {datetime}. An existing file is never overwritten.",
                &p,
            ));
        div()
            .flex()
            .flex_col()
            .gap(px(16.))
            .p(px(18.))
            .child(logging)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        ui::button("ts-save", "Save", Kind::Primary, &p)
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    )
                    .when(self.saved, |d| {
                        d.child(div().text_size(px(12.)).text_color(p.dev).child("Saved"))
                    }),
            )
    }
}

/// The page body for the settings overlay.
pub fn element(view: &Entity<TerminalSettingsView>) -> AnyElement {
    view.clone().into_any_element()
}
