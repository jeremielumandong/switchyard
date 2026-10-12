//! An RDP session in a tab (in-app client). The session runs on the core runtime; this
//! view draws the desktop image it copies from the shared frame buffer when told it
//! changed, and sends keyboard and mouse input as `RdpInput`s. Toolbar: disconnect /
//! reconnect, Ctrl+Alt+Del, fit or actual size.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::AppContext as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Bounds, Context, Corners, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    KeyDownEvent, KeyUpEvent, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement as _, Pixels, Render, RenderImage, ScrollDelta, ScrollWheelEvent,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, canvas, div,
    fill, point, px, size,
};
use switchyard_core::remote::rdp::input::{self as rdp_input, Placement};
use switchyard_core::remote::rdp::{FrameBuffer, KeyMods, RdpInput, ScaleMode};
use switchyard_core::servers::embedded::{RdpCommand, RdpState};
use switchyard_core::store::DesktopServer;
use switchyard_core::{Command, RequestId, RuntimeHandle};

use crate::app_state::next_id;
use crate::appearance::{rpx, ts};
use crate::theme::palette;
use crate::ui::{self, Kind};

/// Where the tab is.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    /// Waiting for the first layout to know the tab's size.
    Idle,
    /// Session state from the runtime.
    Session(RdpState),
}

/// One in-app RDP session.
pub struct RdpTab {
    core: RuntimeHandle,
    /// The remote desktop (name, environment; refreshed when the profile changes).
    pub desktop: DesktopServer,
    session: RequestId,
    phase: Phase,
    image: Option<Arc<RenderImage>>,
    /// The desktop's size.
    remote: (u16, u16),
    scale: ScaleMode,
    /// The image area's bounds from the last paint.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    focus: FocusHandle,
    /// Modifiers as last sent.
    mods: KeyMods,
    /// A connect is scheduled (first layout).
    starting: bool,
    blur: Option<Subscription>,
}

impl RdpTab {
    /// A tab for `desktop`; it connects once laid out.
    pub fn new(core: RuntimeHandle, desktop: DesktopServer, cx: &mut Context<Self>) -> Self {
        Self {
            core,
            desktop,
            session: next_id(),
            phase: Phase::Idle,
            image: None,
            remote: (0, 0),
            scale: ScaleMode::Fit,
            bounds: Rc::new(Cell::new(Bounds::default())),
            focus: cx.focus_handle(),
            mods: KeyMods::default(),
            starting: false,
            blur: None,
        }
    }

    /// Whether `session` is this tab's.
    pub fn owns(&self, session: RequestId) -> bool {
        self.session == session
    }

    fn connected(&self) -> bool {
        matches!(self.phase, Phase::Session(RdpState::Connected { .. }))
    }

    fn running(&self) -> bool {
        matches!(
            self.phase,
            Phase::Session(RdpState::Connecting(_) | RdpState::Connected { .. })
        )
    }

    /// Short status for the toolbar.
    fn status(&self) -> String {
        match &self.phase {
            Phase::Idle => "Starting…".into(),
            Phase::Session(RdpState::Connecting(step)) => format!("{step}…"),
            Phase::Session(RdpState::Connected { width, height }) => {
                format!("Connected · {width}×{height}")
            }
            Phase::Session(RdpState::Ended(why)) => format!("Disconnected · {why}"),
            Phase::Session(RdpState::Failed { message, .. }) => message.clone(),
        }
    }

    fn send(&self, input: Vec<RdpInput>) {
        if !input.is_empty() && self.connected() {
            self.core.send(Command::Rdp(RdpCommand::Input {
                session: self.session,
                input,
            }));
        }
    }

    /// Connect with the tab's size (in device pixels, so the image is drawn 1:1).
    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let b = self.bounds.get();
        let scale = window.scale_factor();
        // The bounds are positive and far below u32::MAX.
        let w = (f32::from(b.size.width) * scale).max(0.0) as u32;
        let h = (f32::from(b.size.height) * scale).max(0.0) as u32;
        self.session = next_id();
        self.image = None;
        self.remote = (0, 0);
        self.phase = Phase::Session(RdpState::Connecting("Starting".into()));
        self.core.send(Command::Rdp(RdpCommand::Open {
            session: self.session,
            id: self.desktop.id.clone(),
            width: w,
            height: h,
        }));
        if self.blur.is_none() {
            // Keys held when focus leaves would stay down on the server.
            self.blur = Some(cx.on_blur(&self.focus, window, |this, _, _| {
                this.send(vec![RdpInput::ReleaseAll]);
                this.mods = KeyMods::default();
            }));
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.core.send(Command::Rdp(RdpCommand::Close {
            session: self.session,
        }));
        cx.notify();
    }

    /// The tab closes: end the session.
    pub fn shutdown(&mut self) {
        if self.running() {
            self.core.send(Command::Rdp(RdpCommand::Close {
                session: self.session,
            }));
        }
    }

    /// The session's state changed.
    pub fn on_state(&mut self, session: RequestId, state: RdpState, cx: &mut Context<Self>) {
        if session != self.session {
            return;
        }
        if let RdpState::Connected { width, height } = state {
            self.remote = (width, height);
        }
        self.phase = Phase::Session(state);
        cx.notify();
    }

    /// The desktop image changed: copy it out and swap the drawn image.
    pub fn on_frame(
        &mut self,
        session: RequestId,
        frames: &FrameBuffer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if session != self.session {
            return;
        }
        let Some(snap) = frames.take() else {
            return;
        };
        self.remote = (snap.width, snap.height);
        let Some(buf) =
            image::RgbaImage::from_raw(u32::from(snap.width), u32::from(snap.height), snap.bgra)
        else {
            return;
        };
        // GPUI images are BGRA, which is what the frame buffer holds.
        let next = Arc::new(RenderImage::new(vec![image::Frame::new(buf)]));
        if let Some(old) = self.image.replace(next) {
            cx.drop_image(old, Some(window));
        }
        cx.notify();
    }

    fn placement(&self) -> Placement {
        let b = self.bounds.get();
        rdp_input::place(
            f32::from(b.size.width),
            f32::from(b.size.height),
            self.remote.0,
            self.remote.1,
            self.scale,
        )
    }

    fn remote_point(&self, pos: gpui_kit::Point<Pixels>) -> (u16, u16) {
        let b = self.bounds.get();
        rdp_input::to_remote(
            self.placement(),
            f32::from(pos.x - b.origin.x),
            f32::from(pos.y - b.origin.y),
            self.remote.0,
            self.remote.1,
        )
    }

    fn key_mods(m: &gpui_kit::Modifiers) -> KeyMods {
        KeyMods {
            ctrl: m.control,
            alt: m.alt,
            shift: m.shift,
            meta: m.platform,
        }
    }

    fn sync_mods(&mut self, m: &gpui_kit::Modifiers) {
        let now = Self::key_mods(m);
        let changes = rdp_input::modifier_changes(self.mods, now);
        self.mods = now;
        self.send(changes);
    }

    fn key_down(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) {
        if !self.connected() {
            return;
        }
        self.sync_mods(&ev.keystroke.modifiers);
        let k = &ev.keystroke;
        self.send(rdp_input::key_down(
            &k.key,
            k.key_char.as_deref(),
            self.mods,
        ));
        cx.stop_propagation();
    }

    fn key_up(&mut self, ev: &KeyUpEvent, cx: &mut Context<Self>) {
        if !self.connected() {
            return;
        }
        let k = &ev.keystroke;
        self.send(rdp_input::key_up(&k.key, k.key_char.as_deref(), self.mods));
        self.sync_mods(&k.modifiers);
        cx.stop_propagation();
    }

    fn button(b: MouseButton) -> Option<rdp_input::MouseButton> {
        Some(match b {
            MouseButton::Left => rdp_input::MouseButton::Left,
            MouseButton::Right => rdp_input::MouseButton::Right,
            MouseButton::Middle => rdp_input::MouseButton::Middle,
            MouseButton::Navigate(gpui_kit::NavigationDirection::Back) => {
                rdp_input::MouseButton::X1
            }
            MouseButton::Navigate(gpui_kit::NavigationDirection::Forward) => {
                rdp_input::MouseButton::X2
            }
        })
    }

    fn mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        if !self.connected() {
            return;
        }
        let (x, y) = self.remote_point(ev.position);
        let mut input = vec![RdpInput::MouseMove { x, y }];
        if let Some(button) = Self::button(ev.button) {
            input.push(RdpInput::Button { button, down: true });
        }
        self.send(input);
    }

    fn mouse_up(&mut self, ev: &MouseUpEvent) {
        if !self.connected() {
            return;
        }
        let (x, y) = self.remote_point(ev.position);
        let mut input = vec![RdpInput::MouseMove { x, y }];
        if let Some(button) = Self::button(ev.button) {
            input.push(RdpInput::Button {
                button,
                down: false,
            });
        }
        self.send(input);
    }

    fn mouse_move(&mut self, ev: &MouseMoveEvent) {
        if !self.connected() {
            return;
        }
        let (x, y) = self.remote_point(ev.position);
        self.send(vec![RdpInput::MouseMove { x, y }]);
    }

    fn wheel(&mut self, ev: &ScrollWheelEvent) {
        if !self.connected() {
            return;
        }
        let (dx, dy) = match ev.delta {
            ScrollDelta::Lines(p) => (p.x, p.y),
            // About 20 px per line, like most toolkits.
            ScrollDelta::Pixels(p) => (f32::from(p.x) / 20.0, f32::from(p.y) / 20.0),
        };
        let mut input = Vec::new();
        let v = rdp_input::wheel_units(dy);
        if v != 0 {
            input.push(RdpInput::Wheel {
                vertical: true,
                units: v,
            });
        }
        let h = rdp_input::wheel_units(dx);
        if h != 0 {
            input.push(RdpInput::Wheel {
                vertical: false,
                units: h,
            });
        }
        self.send(input);
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let failed_cert = matches!(
            self.phase,
            Phase::Session(RdpState::Failed {
                cert_changed: true,
                ..
            })
        );
        let error = matches!(self.phase, Phase::Session(RdpState::Failed { .. }));
        div()
            .h(rpx(30.))
            .flex_none()
            .flex()
            .items_center()
            .gap(rpx(6.))
            .px(rpx(10.))
            .border_b_1()
            .border_color(p.bd)
            .bg(p.panel)
            .text_size(ts::BODY)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(if error { p.prod } else { p.fg2 })
                    .child(SharedString::from(self.status())),
            )
            .when(failed_cert, |d| {
                let id = self.desktop.id.clone();
                d.child(
                    ui::button(
                        "rdp-forget-pin",
                        "Forget pinned certificate",
                        Kind::Secondary,
                        &p,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.core
                            .send(Command::Rdp(RdpCommand::ForgetPin { id: id.clone() }));
                        cx.notify();
                    })),
                )
            })
            .child(
                ui::button("rdp-cad", "Ctrl+Alt+Del", Kind::Ghost, &p)
                    .on_click(cx.listener(|this, _, _, _| this.send(rdp_input::ctrl_alt_del()))),
            )
            .child(
                ui::button(
                    "rdp-scale",
                    match self.scale {
                        ScaleMode::Fit => "Fit",
                        ScaleMode::Actual => "1:1",
                    },
                    Kind::Ghost,
                    &p,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.scale = match this.scale {
                        ScaleMode::Fit => ScaleMode::Actual,
                        ScaleMode::Actual => ScaleMode::Fit,
                    };
                    cx.notify();
                })),
            )
            .child(if self.running() {
                ui::button("rdp-disconnect", "Disconnect", Kind::Secondary, &p)
                    .on_click(cx.listener(|this, _, _, cx| this.disconnect(cx)))
            } else {
                ui::button("rdp-reconnect", "Reconnect", Kind::Primary, &p)
                    .on_click(cx.listener(|this, _, w, cx| this.connect(w, cx)))
            })
    }
}

impl Focusable for RdpTab {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for RdpTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        // Connect once the image area has a size.
        if self.phase == Phase::Idle && !self.starting {
            if self.bounds.get().size.width > px(0.) {
                // Not during render: connecting moves the focus.
                self.starting = true;
                cx.defer_in(window, |this, window, cx| {
                    this.starting = false;
                    if this.phase == Phase::Idle {
                        this.connect(window, cx);
                    }
                });
            } else {
                cx.on_next_frame(window, |_, _, cx| cx.notify());
            }
        }
        let bounds = self.bounds.clone();
        let image = self.image.clone();
        let placement = self.placement();
        let bg = p.term;
        let surface = canvas(
            move |b, _, _| bounds.set(b),
            move |b, (), window, _| {
                window.paint_quad(fill(b, bg));
                if let Some(img) = image {
                    let at = Bounds {
                        origin: point(b.origin.x + px(placement.x), b.origin.y + px(placement.y)),
                        size: size(px(placement.w), px(placement.h)),
                    };
                    let _ = window.paint_image(b, at, Corners::default(), img, 0, false);
                }
            },
        )
        .size_full();
        let overlay = (!self.connected()).then(|| {
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .text_size(ts::UI)
                .text_color(p.fg2)
                .child(SharedString::from(match &self.phase {
                    Phase::Session(RdpState::Failed { message, .. }) => {
                        format!("Could not connect to {}: {message}", self.desktop.name)
                    }
                    _ => format!("{} · {}", self.desktop.name, self.status()),
                }))
        });
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(p.surface)
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .id("rdp-desktop")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .track_focus(&self.focus)
                    .key_context("RemoteDesktop")
                    .on_key_down(
                        cx.listener(|this, ev: &KeyDownEvent, _, cx| this.key_down(ev, cx)),
                    )
                    .on_key_up(cx.listener(|this, ev: &KeyUpEvent, _, cx| this.key_up(ev, cx)))
                    .on_modifiers_changed(cx.listener(|this, ev: &ModifiersChangedEvent, _, _| {
                        if this.connected() {
                            this.sync_mods(&ev.modifiers);
                        }
                    }))
                    .on_any_mouse_down(
                        cx.listener(|this, ev: &MouseDownEvent, w, cx| this.mouse_down(ev, w, cx)),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, ev, _, _| this.mouse_up(ev)),
                    )
                    .on_mouse_up(
                        MouseButton::Right,
                        cx.listener(|this, ev, _, _| this.mouse_up(ev)),
                    )
                    .on_mouse_up(
                        MouseButton::Middle,
                        cx.listener(|this, ev, _, _| this.mouse_up(ev)),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, ev, _, _| this.mouse_up(ev)),
                    )
                    .on_mouse_move(cx.listener(|this, ev, _, _| this.mouse_move(ev)))
                    .on_scroll_wheel(cx.listener(|this, ev, _, _| this.wheel(ev)))
                    .child(surface)
                    .children(overlay),
            )
    }
}

impl crate::workspace::Workspace {
    /// Open remote desktop `id` in a tab (or bring its tab to the front).
    pub(crate) fn open_rdp_tab(
        &mut self,
        id: &switchyard_core::store::ProfileId,
        cx: &mut Context<Self>,
    ) {
        use crate::workspace::Tab;
        use switchyard_core::store::Profile;
        if let Some(ix) = self
            .tabs
            .iter()
            .position(|t| matches!(t, Tab::Rdp(r) if r.read(cx).desktop.id == *id))
        {
            self.activate(ix, cx);
            return;
        }
        let Some(desktop) = self.profiles.all.iter().find_map(|p| match p {
            Profile::Desktop(d) if d.id == *id => Some(d.clone()),
            _ => None,
        }) else {
            return;
        };
        let core = self.core.clone();
        let tab = cx.new(|cx| RdpTab::new(core, desktop, cx));
        self.tabs.push(Tab::Rdp(tab));
        self.activate(self.tabs.len() - 1, cx);
    }

    /// Route [`switchyard_core::Event::Rdp`] to its tab; a session without one is closed.
    pub(crate) fn on_rdp_event(
        &mut self,
        ev: switchyard_core::servers::embedded::RdpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::workspace::Tab;
        use switchyard_core::servers::embedded::RdpEvent;
        let session = match &ev {
            RdpEvent::State { session, .. } | RdpEvent::Frame { session, .. } => *session,
        };
        let tab = self.tabs.iter().find_map(|t| match t {
            Tab::Rdp(r) if r.read(cx).owns(session) => Some(r.clone()),
            _ => None,
        });
        match (tab, ev) {
            (Some(t), RdpEvent::State { state, .. }) => {
                t.update(cx, |t, cx| t.on_state(session, state, cx));
            }
            (Some(t), RdpEvent::Frame { frames, .. }) => {
                t.update(cx, |t, cx| t.on_frame(session, &frames, window, cx));
            }
            (None, RdpEvent::State { state, .. }) => {
                if matches!(state, RdpState::Connecting(_) | RdpState::Connected { .. }) {
                    self.core.send(Command::Rdp(RdpCommand::Close { session }));
                }
            }
            (None, RdpEvent::Frame { .. }) => {
                self.core.send(Command::Rdp(RdpCommand::Close { session }));
            }
        }
    }
}
