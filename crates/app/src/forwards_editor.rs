//! The Host editor's port forwards (MobaXterm's SSH tunnels): one row per saved forward
//! with its direction, the address and port it listens on, its target and auto-start.

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _,
    IntoElement as _, ParentElement as _, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, div, px,
};
use switchyard_core::store::{ForwardDirection, PortForward};

use crate::theme::{MONO, Palette};
use crate::ui;

/// One forward being edited.
pub(crate) struct ForwardRow {
    id: String,
    direction: ForwardDirection,
    /// Kept as saved (named in the Tunnels panel; not edited here yet).
    name: String,
    bind_address: Entity<InputState>,
    bind_port: Entity<InputState>,
    target_host: Entity<InputState>,
    target_port: Entity<InputState>,
    auto_start: bool,
}

fn input<T: 'static>(
    window: &mut Window,
    cx: &mut Context<T>,
    value: String,
    placeholder: &'static str,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .default_value(value)
    })
}

fn port_text(p: u16) -> String {
    if p == 0 { String::new() } else { p.to_string() }
}

impl ForwardRow {
    pub(crate) fn new<T: 'static>(
        f: &PortForward,
        window: &mut Window,
        cx: &mut Context<T>,
    ) -> Self {
        Self {
            id: f.id.clone(),
            direction: f.direction,
            name: f.name.clone(),
            bind_address: input(window, cx, f.bind_address.clone(), "127.0.0.1"),
            bind_port: input(window, cx, port_text(f.bind_port), "port"),
            target_host: input(window, cx, f.target_host.clone(), "host"),
            target_port: input(window, cx, port_text(f.target_port), "port"),
            auto_start: f.auto_start,
        }
    }

    pub(crate) fn set_direction(&mut self, d: ForwardDirection) {
        self.direction = d;
    }

    pub(crate) fn toggle_auto(&mut self) {
        self.auto_start = !self.auto_start;
    }

    /// The forward as entered, or what is wrong with it.
    pub(crate) fn read<T>(&self, cx: &Context<T>) -> Result<PortForward, String> {
        let text = |e: &Entity<InputState>| e.read(cx).value().trim().to_owned();
        let port = |e: &Entity<InputState>, what: &str| -> Result<u16, String> {
            let t = text(e);
            if t.is_empty() {
                return Ok(0);
            }
            t.parse::<u16>()
                .map_err(|_| format!("{what} must be a number from 1 to 65535"))
        };
        let f = PortForward {
            id: self.id.clone(),
            name: self.name.clone(),
            direction: self.direction,
            bind_address: text(&self.bind_address),
            bind_port: port(&self.bind_port, "the listening port")?,
            target_host: text(&self.target_host),
            target_port: port(&self.target_port, "the target port")?,
            auto_start: self.auto_start,
        };
        f.validate().map_err(|e| format!("{}: {e}", f.summary()))?;
        Ok(f)
    }
}

/// What a row asks its editor to do.
pub(crate) enum RowAction {
    Direction(usize, ForwardDirection),
    ToggleAuto(usize),
    Remove(usize),
    Add,
}

fn small_input(e: &Entity<InputState>, w: f32, p: &Palette) -> gpui_kit::Div {
    div()
        .w(px(w))
        .flex_none()
        .h(px(26.))
        .flex()
        .items_center()
        .px(px(7.))
        .border_1()
        .border_color(p.bd2)
        .rounded(px(5.))
        .bg(p.bg)
        .font_family(MONO)
        .text_size(px(12.))
        .child(Input::new(e).appearance(false).text_size(px(12.)))
}

/// The forwards section. `on` turns a row action into an editor update.
pub(crate) fn render<T: 'static>(
    rows: &[ForwardRow],
    p: &Palette,
    cx: &mut Context<T>,
    on: impl Fn(&mut T, RowAction, &mut Window, &mut Context<T>) + Clone + 'static,
) -> AnyElement {
    let header = div()
        .flex()
        .items_center()
        .gap(px(8.))
        .child(
            div()
                .text_size(px(11.5))
                .text_color(p.fg2)
                .font_weight(FontWeight::MEDIUM)
                .child("Port forwarding"),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(p.fg3)
                .child("L: listen here · R: listen on the Host · D: SOCKS proxy here"),
        )
        .child(div().flex_1())
        .child({
            let on = on.clone();
            div()
                .id("fwd-add")
                .px(px(8.))
                .py(px(2.))
                .rounded(px(5.))
                .text_size(px(12.))
                .text_color(p.acc)
                .hover(|s| s.bg(p.hover))
                .on_click(cx.listener(move |this, _, w, cx| on(this, RowAction::Add, w, cx)))
                .child("+ Add forward")
        });
    let list =
        rows.iter().enumerate().map(|(i, r)| {
            let segment = |d: ForwardDirection, label: &'static str| {
                let active = r.direction == d;
                let on = on.clone();
                div()
                    .id(SharedString::from(format!("fwd-{i}-{label}")))
                    .w(px(22.))
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .font_family(MONO)
                    .text_size(px(11.5))
                    .when(active, |d| d.bg(p.sel).text_color(p.fg))
                    .when(!active, |d| d.text_color(p.fg3).hover(|s| s.bg(p.hover)))
                    .on_click(cx.listener(move |this, _, w, cx| {
                        on(this, RowAction::Direction(i, d), w, cx)
                    }))
                    .child(label)
            };
            let arrow = match r.direction {
                ForwardDirection::Local => "→",
                ForwardDirection::Remote => "←",
                ForwardDirection::Dynamic => "",
            };
            let (on_auto, on_remove) = (on.clone(), on.clone());
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .border_1()
                        .border_color(p.bd2)
                        .rounded(px(5.))
                        .overflow_hidden()
                        .child(segment(ForwardDirection::Local, "L"))
                        .child(segment(ForwardDirection::Remote, "R"))
                        .child(segment(ForwardDirection::Dynamic, "D")),
                )
                .child(small_input(&r.bind_address, 92., p))
                .child(div().text_color(p.fg3).child(":"))
                .child(small_input(&r.bind_port, 58., p))
                .when(r.direction != ForwardDirection::Dynamic, |d| {
                    d.child(div().w(px(12.)).text_color(p.fg3).child(arrow))
                        .child(small_input(&r.target_host, 110., p))
                        .child(div().text_color(p.fg3).child(":"))
                        .child(small_input(&r.target_port, 58., p))
                })
                .child(
                    ui::checkbox(
                        SharedString::from(format!("fwd-{i}-auto")),
                        r.auto_start,
                        "Auto",
                        p,
                    )
                    .on_click(cx.listener(move |this, _, w, cx| {
                        on_auto(this, RowAction::ToggleAuto(i), w, cx)
                    })),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .id(SharedString::from(format!("fwd-{i}-remove")))
                        .px(px(6.))
                        .text_color(p.fg3)
                        .hover(|s| s.text_color(p.prod))
                        .on_click(cx.listener(move |this, _, w, cx| {
                            on_remove(this, RowAction::Remove(i), w, cx)
                        }))
                        .child("×"),
                )
        });
    div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .child(header)
        .when(rows.is_empty(), |d| {
            d.child(
                div()
                    .text_size(px(11.5))
                    .text_color(p.fg3)
                    .child("No forwards. Running ones show in the Tunnels panel, where saved ones can be started."),
            )
        })
        .children(list)
        .into_any_element()
}
