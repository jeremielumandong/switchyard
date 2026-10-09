//! Terminal tab: one or more panes, each drawing a [`Terminal`] snapshot on a canvas.
//!
//! Output is parsed off the UI thread; a pane only copies the visible screen when the
//! runtime says something changed. Keys, mouse reports and pastes are encoded by
//! `switchyard-term` and sent to the runtime as bytes.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AppContext as _, Bounds, ClipboardItem, Context, Entity, FocusHandle, Focusable, Font,
    FontStyle, FontWeight, Hsla, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Point, Render,
    ScrollDelta, ScrollWheelEvent, SharedString, StatefulInteractiveElement as _,
    StrikethroughStyle, Styled as _, Subscription, TextAlign, TextRun, UnderlineStyle, WeakEntity,
    Window, canvas, div, fill, font, outline, point, px, size,
};
use switchyard_core::store::{EnvironmentLabel, ProfileId};
use switchyard_core::term::input::{
    Key, Mods, MouseAction, MouseButton as TermButton, encode_key, encode_mouse, encode_paste,
};
use switchyard_core::term::links::url_at;
use switchyard_core::term::{CursorShape, Mark, Snapshot, TermColor, TermSize, Terminal};
use switchyard_core::{Command, RuntimeHandle, TermId, TermStatus, TermTarget};

use crate::actions::{TermCopy, TermFind, TermPaste, TermSplit};
use crate::app_state::next_id;
use crate::theme::{MONO, Palette, palette};
use crate::ui::{self, Kind};

const PAD_X: f32 = 12.;
const PAD_Y: f32 = 10.;
const MAX_PANES: usize = 4;

/// Where a pane's cells are on screen, measured during paint.
#[derive(Clone, Copy, Debug, Default)]
struct Geom {
    origin: Point<Pixels>,
    cell_w: f32,
    /// Line height (follows the editor font size and zoom).
    line_h: f32,
    cols: u16,
    rows: u16,
}

#[derive(Clone, Debug, PartialEq)]
enum PaneState {
    Connecting,
    Live,
    Reconnecting { attempt: u32, of: u32, in_secs: u64 },
    Exited(Option<u32>),
    Failed(String),
    Blocked(Box<ChangedKey>),
}

/// A host key mismatch that blocks the connection.
#[derive(Clone, Debug, PartialEq)]
pub struct ChangedKey {
    /// Host profile.
    pub host_id: ProfileId,
    /// Host label.
    pub host: String,
    /// `address:port`.
    pub address: String,
    /// Stored fingerprint.
    pub stored: String,
    /// Received fingerprint.
    pub received: String,
    /// `file:line` of the stored key.
    pub location: String,
}

struct Pane {
    id: TermId,
    terminal: Option<Terminal>,
    state: PaneState,
    title: String,
    description: String,
    size: TermSize,
    geom: Rc<Cell<Geom>>,
    focus: FocusHandle,
    snapshot: Rc<Snapshot>,
    selecting: bool,
    pressed: Option<TermButton>,
}

/// A terminal tab.
pub struct TerminalTab {
    /// Tab title (Host name or "Local shell").
    pub title: SharedString,
    /// Environment of the Host.
    pub env: EnvironmentLabel,
    core: RuntimeHandle,
    target: TermTarget,
    panes: Vec<Pane>,
    active: usize,
    broadcast: bool,
    search: Option<Entity<InputState>>,
    _search_sub: Option<Subscription>,
    /// Typed into the first pane once its shell is up (e.g. `cd` to a folder).
    startup: Option<Vec<u8>>,
    /// A coding CLI with Switchyard's tools ("Open in terminal"): (CLI, connection).
    agent: Option<(
        Option<switchyard_core::agents::AgentKind>,
        Option<ProfileId>,
    )>,
}

impl TerminalTab {
    /// The Host this terminal is on (`None` for a local shell).
    pub fn host_id(&self) -> Option<&ProfileId> {
        match &self.target {
            TermTarget::Host(id) => Some(id),
            TermTarget::Local { .. } => None,
        }
    }

    /// A terminal for a Host or the local machine; the first pane connects right away.
    pub fn new(
        core: RuntimeHandle,
        title: String,
        env: EnvironmentLabel,
        host: Option<ProfileId>,
        cx: &mut Context<Self>,
    ) -> Self {
        let target = match host {
            Some(h) => TermTarget::Host(h),
            None => TermTarget::Local { profile: None },
        };
        let mut this = Self {
            title: title.into(),
            env,
            core,
            target,
            panes: Vec::new(),
            active: 0,
            broadcast: false,
            search: None,
            _search_sub: None,
            startup: None,
            agent: None,
        };
        this.add_pane(cx);
        this
    }

    /// A coding CLI running interactively with Switchyard's tools attached; `agent` `None`
    /// is the configured one, `connection` the one it may use (all agent-enabled ones when
    /// `None`).
    pub fn new_agent(
        core: RuntimeHandle,
        title: String,
        agent: Option<switchyard_core::agents::AgentKind>,
        connection: Option<ProfileId>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            title: title.into(),
            env: EnvironmentLabel::Local,
            core,
            target: TermTarget::Local { profile: None },
            panes: Vec::new(),
            active: 0,
            broadcast: false,
            search: None,
            _search_sub: None,
            startup: None,
            agent: Some((agent, connection)),
        };
        this.add_pane(cx);
        this
    }

    /// Type `input` into the first shell once it is up.
    pub fn with_startup(mut self, input: impl Into<Vec<u8>>) -> Self {
        self.startup = Some(input.into());
        self
    }

    fn run_startup(&mut self, term: TermId) {
        if self.panes.first().map(|p| p.id) == Some(term)
            && let Some(bytes) = self.startup.take()
        {
            self.core.send(Command::TerminalInput { term, bytes });
        }
    }

    /// The Host this terminal is on, if remote.
    pub fn host(&self) -> Option<&ProfileId> {
        match &self.target {
            TermTarget::Host(h) => Some(h),
            TermTarget::Local { .. } => None,
        }
    }

    /// Whether the shell runs on a Host.
    pub fn is_remote(&self) -> bool {
        matches!(self.target, TermTarget::Host(_))
    }

    /// Whether this tab owns terminal `term`.
    pub fn owns(&self, term: TermId) -> bool {
        self.panes.iter().any(|p| p.id == term)
    }

    /// Short status for the status bar.
    pub fn status(&self) -> String {
        match self.panes.get(self.active).map(|p| &p.state) {
            Some(PaneState::Live) => {
                let p = &self.panes[self.active];
                format!("{} × {}", p.size.cols, p.size.rows)
            }
            Some(PaneState::Connecting) => "Connecting".into(),
            Some(PaneState::Reconnecting { .. }) => "Reconnecting".into(),
            Some(PaneState::Exited(_)) => "Exited".into(),
            Some(PaneState::Failed(_)) => "Failed".into(),
            Some(PaneState::Blocked(_)) => "Blocked".into(),
            None => String::new(),
        }
    }

    fn add_pane(&mut self, cx: &mut Context<Self>) {
        let id = next_id();
        let size = self
            .panes
            .get(self.active)
            .map(|p| p.size)
            .unwrap_or(TermSize {
                cols: 100,
                rows: 30,
            });
        match &self.agent {
            Some((agent, connection)) => self.core.send(Command::OpenAgentTerminal {
                term: id,
                agent: *agent,
                connection: connection.clone(),
                size,
            }),
            None => self.core.send(Command::OpenTerminal {
                term: id,
                target: self.target.clone(),
                size,
            }),
        }
        self.panes.push(Pane {
            id,
            terminal: None,
            state: PaneState::Connecting,
            title: String::new(),
            description: String::new(),
            size,
            geom: Rc::default(),
            focus: cx.focus_handle(),
            snapshot: Rc::default(),
            selecting: false,
            pressed: None,
        });
        self.active = self.panes.len() - 1;
    }

    fn pane_mut(&mut self, term: TermId) -> Option<&mut Pane> {
        self.panes.iter_mut().find(|p| p.id == term)
    }

    /// The runtime opened terminal `term`.
    pub fn on_opened(
        &mut self,
        term: TermId,
        terminal: Terminal,
        description: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let active = self.panes.get(self.active).map(|p| p.id) == Some(term);
        let remote = self.is_remote();
        if let Some(p) = self.pane_mut(term) {
            p.snapshot = Rc::new(terminal.snapshot());
            p.terminal = Some(terminal);
            // SSH terminals report Connected separately (after prompts and login).
            if !remote {
                p.state = PaneState::Live;
            }
            p.description = description;
            if active {
                p.focus.focus(window, cx);
            }
        }
        if !remote {
            self.run_startup(term);
        }
        cx.notify();
    }

    /// Connection state of an SSH terminal.
    pub fn on_status(&mut self, term: TermId, status: TermStatus, cx: &mut Context<Self>) {
        if let Some(p) = self.pane_mut(term) {
            match status {
                TermStatus::Connecting => match &mut p.state {
                    // Retrying now: keep the banner, drop the countdown.
                    PaneState::Reconnecting { in_secs, .. } => *in_secs = 0,
                    state => *state = PaneState::Connecting,
                },
                TermStatus::Connected { description } => {
                    p.state = PaneState::Live;
                    p.description = description;
                    self.run_startup(term);
                }
                TermStatus::Reconnecting {
                    attempt,
                    of,
                    in_secs,
                } => {
                    p.state = PaneState::Reconnecting {
                        attempt,
                        of,
                        in_secs,
                    };
                }
            }
        }
        cx.notify();
    }

    /// The server's host key changed; show the warning instead of the terminal.
    pub fn on_host_key_changed(&mut self, term: TermId, key: ChangedKey, cx: &mut Context<Self>) {
        if let Some(p) = self.pane_mut(term) {
            p.state = PaneState::Blocked(Box::new(key));
        }
        cx.notify();
    }

    /// Opening failed.
    pub fn on_failed(&mut self, term: TermId, message: String, cx: &mut Context<Self>) {
        if let Some(p) = self.pane_mut(term) {
            p.state = PaneState::Failed(message);
        }
        cx.notify();
    }

    /// New output.
    pub fn on_wake(&mut self, term: TermId, cx: &mut Context<Self>) {
        if self.owns(term) {
            cx.notify();
        }
    }

    /// Title change.
    pub fn on_title(&mut self, term: TermId, title: String, cx: &mut Context<Self>) {
        if let Some(p) = self.pane_mut(term) {
            p.title = title;
        }
        cx.notify();
    }

    /// The program ended.
    pub fn on_exited(
        &mut self,
        term: TermId,
        code: Option<u32>,
        message: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(p) = self.pane_mut(term) {
            // The blocked-key screen stays up; the exit only confirms it.
            if matches!(p.state, PaneState::Blocked(_)) {
                cx.notify();
                return;
            }
            p.state = match message {
                Some(m) => PaneState::Failed(m),
                None => PaneState::Exited(code),
            };
            if let Some(t) = &p.terminal {
                p.snapshot = Rc::new(t.snapshot());
            }
        }
        cx.notify();
    }

    /// Close every terminal (tab closed).
    pub fn shutdown(&mut self) {
        for p in &self.panes {
            self.core.send(Command::CloseTerminal { term: p.id });
        }
    }

    fn reconnect(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(old) = self.panes.get(ix).map(|p| p.id) else {
            return;
        };
        self.core.send(Command::CloseTerminal { term: old });
        let id = next_id();
        if let Some(p) = self.panes.get_mut(ix) {
            p.id = id;
            p.terminal = None;
            p.state = PaneState::Connecting;
            p.snapshot = Rc::default();
            self.core.send(Command::OpenTerminal {
                term: id,
                target: self.target.clone(),
                size: p.size,
            });
        }
        cx.notify();
    }

    fn split(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.panes.len() >= MAX_PANES {
            return;
        }
        self.add_pane(cx);
        if let Some(p) = self.panes.last() {
            p.focus.focus(window, cx);
        }
        cx.notify();
    }

    fn close_pane(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.panes.len() <= 1 || ix >= self.panes.len() {
            return;
        }
        let p = self.panes.remove(ix);
        self.core.send(Command::CloseTerminal { term: p.id });
        self.active = self.active.min(self.panes.len() - 1);
        cx.notify();
    }

    /// Send bytes typed in pane `ix` (to every live pane when broadcasting).
    fn send_input(&mut self, ix: usize, bytes: Vec<u8>) {
        let targets: Vec<TermId> = if self.broadcast {
            self.panes
                .iter()
                .filter(|p| p.state == PaneState::Live)
                .map(|p| p.id)
                .collect()
        } else {
            self.panes.get(ix).map(|p| p.id).into_iter().collect()
        };
        for p in &self.panes {
            if targets.contains(&p.id)
                && let Some(t) = &p.terminal
            {
                t.clear_selection();
                if p.snapshot.display_offset > 0 {
                    t.scroll_to_bottom();
                }
            }
        }
        for term in targets {
            self.core.send(Command::TerminalInput {
                term,
                bytes: bytes.clone(),
            });
        }
    }

    fn resize_pane(&mut self, term: TermId, size: TermSize, cx: &mut Context<Self>) {
        let Some(p) = self.pane_mut(term) else { return };
        if p.size == size {
            return;
        }
        p.size = size;
        if let Some(t) = &p.terminal {
            t.resize(size);
        }
        self.core.send(Command::TerminalResize { term, size });
        cx.notify();
    }

    fn copy(&self, ix: usize, cx: &mut Context<Self>) {
        if let Some(text) = self
            .panes
            .get(ix)
            .and_then(|p| p.terminal.as_ref())
            .and_then(|t| t.selection_text())
        {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) else {
            return;
        };
        let modes = self
            .panes
            .get(ix)
            .map(|p| p.snapshot.modes)
            .unwrap_or_default();
        self.send_input(ix, encode_paste(&text, modes));
    }

    fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(s) = &self.search {
            s.update(cx, |s, cx| s.focus(window, cx));
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search scrollback"));
        input.update(cx, |i, cx| i.focus(window, cx));
        let sub = cx.subscribe_in(
            &input,
            window,
            |this, input, ev: &InputEvent, window, cx| {
                match ev {
                    InputEvent::Change => {
                        let q = input.read(cx).value().to_string();
                        if let Some(t) = this.active_terminal() {
                            t.search(&q);
                        }
                        cx.notify();
                    }
                    InputEvent::PressEnter { shift, .. } => {
                        // Enter walks back through older output; Shift+Enter forward.
                        if let Some(t) = this.active_terminal() {
                            t.search_step(*shift);
                        }
                        cx.notify();
                    }
                    InputEvent::Blur => {}
                    _ => {}
                }
                let _ = window;
            },
        );
        self.search = Some(input);
        self._search_sub = Some(sub);
        cx.notify();
    }

    fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search = None;
        self._search_sub = None;
        if let Some(p) = self.panes.get(self.active) {
            if let Some(t) = &p.terminal {
                t.clear_search();
            }
            p.focus.focus(window, cx);
        }
        cx.notify();
    }

    fn refocus(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = self.panes.get(self.active) {
            p.focus.focus(window, cx);
        }
    }

    fn active_terminal(&self) -> Option<&Terminal> {
        self.panes
            .get(self.active)
            .and_then(|p| p.terminal.as_ref())
    }

    fn cell_at(&self, ix: usize, pos: Point<Pixels>) -> Option<(u16, u16, bool)> {
        let g = self.panes.get(ix)?.geom.get();
        if g.cell_w <= 0. || g.cols == 0 {
            return None;
        }
        let x = f32::from(pos.x - g.origin.x);
        let y = f32::from(pos.y - g.origin.y);
        let col = (x / g.cell_w).floor().clamp(0., f32::from(g.cols - 1));
        let row = (y / g.line_h.max(1.))
            .floor()
            .clamp(0., f32::from(g.rows.max(1) - 1));
        let right = x - col * g.cell_w > g.cell_w / 2.;
        Some((row as u16, col as u16, right))
    }

    fn on_key(
        &mut self,
        ix: usize,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ks = &ev.keystroke;
        let m = &ks.modifiers;
        // App shortcuts: Cmd+… on macOS, Ctrl+Shift+… elsewhere.
        let app_mod = if cfg!(target_os = "macos") {
            m.platform
        } else {
            m.control && m.shift
        };
        // App shortcuts arrive as actions (see `actions::init`); leave the rest of them
        // to the workspace. Keys meant for a child (the search box) are not ours either.
        if app_mod || !self.panes[ix].focus.is_focused(window) {
            return;
        }
        if m.platform {
            return;
        }
        let key = match ks.key.as_str() {
            "enter" => Key::Enter,
            "tab" => Key::Tab,
            "backspace" => Key::Backspace,
            "escape" => Key::Escape,
            "up" => Key::Up,
            "down" => Key::Down,
            "left" => Key::Left,
            "right" => Key::Right,
            "home" => Key::Home,
            "end" => Key::End,
            "pageup" if m.shift => {
                self.scroll(ix, i32::from(self.panes[ix].size.rows / 2), cx);
                cx.stop_propagation();
                return;
            }
            "pagedown" if m.shift => {
                self.scroll(ix, -i32::from(self.panes[ix].size.rows / 2), cx);
                cx.stop_propagation();
                return;
            }
            "pageup" => Key::PageUp,
            "pagedown" => Key::PageDown,
            "insert" => Key::Insert,
            "delete" => Key::Delete,
            "space" => Key::Char(' '),
            k if k.len() > 1 && k.starts_with('f') && k[1..].parse::<u8>().is_ok() => {
                Key::F(k[1..].parse().unwrap_or(0))
            }
            k => {
                let ch = if m.control || m.alt {
                    k.chars().next()
                } else {
                    ks.key_char
                        .as_deref()
                        .and_then(|s| s.chars().next())
                        .or_else(|| (k.chars().count() == 1).then(|| k.chars().next()).flatten())
                };
                match ch {
                    Some(c) => Key::Char(c),
                    None => return,
                }
            }
        };
        let mods = Mods {
            shift: m.shift,
            alt: m.alt,
            ctrl: m.control,
        };
        let modes = self.panes[ix].snapshot.modes;
        if let Some(bytes) = encode_key(&key, mods, modes) {
            self.send_input(ix, bytes);
            cx.stop_propagation();
        }
    }

    fn scroll(&mut self, ix: usize, lines: i32, cx: &mut Context<Self>) {
        if let Some(t) = self.panes.get(ix).and_then(|p| p.terminal.as_ref()) {
            t.scroll(lines);
            cx.notify();
        }
    }

    fn mods(m: &gpui_kit::Modifiers) -> Mods {
        Mods {
            shift: m.shift,
            alt: m.alt,
            ctrl: m.control,
        }
    }

    fn button(b: MouseButton) -> Option<TermButton> {
        match b {
            MouseButton::Left => Some(TermButton::Left),
            MouseButton::Middle => Some(TermButton::Middle),
            MouseButton::Right => Some(TermButton::Right),
            _ => None,
        }
    }

    fn mouse_down(
        &mut self,
        ix: usize,
        ev: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active = ix;
        if let Some(p) = self.panes.get(ix) {
            p.focus.focus(window, cx);
        }
        let Some((row, col, right)) = self.cell_at(ix, ev.position) else {
            return;
        };
        let modes = self.panes[ix].snapshot.modes;
        let follow_link = if cfg!(target_os = "macos") {
            ev.modifiers.platform
        } else {
            ev.modifiers.control
        };
        if follow_link
            && ev.button == MouseButton::Left
            && let Some(t) = &self.panes[ix].terminal
            && let Some(url) = url_at(&t.row_text(row), col as usize)
        {
            cx.open_url(&url);
            return;
        }
        // Shift bypasses mouse reporting so text can still be selected.
        if modes.mouse && !ev.modifiers.shift {
            if let Some(b) = Self::button(ev.button)
                && let Some(bytes) = encode_mouse(
                    MouseAction::Press,
                    b,
                    col,
                    row,
                    Self::mods(&ev.modifiers),
                    modes,
                )
            {
                self.panes[ix].pressed = Some(b);
                self.send_input(ix, bytes);
            }
            return;
        }
        match ev.button {
            MouseButton::Left => {
                if let Some(t) = &self.panes[ix].terminal {
                    t.start_selection(row, col, right, ev.click_count.min(3) as u8);
                }
                self.panes[ix].selecting = true;
                cx.notify();
            }
            MouseButton::Middle => self.paste(ix, cx),
            _ => {}
        }
    }

    fn mouse_move(&mut self, ix: usize, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some((row, col, right)) = self.cell_at(ix, ev.position) else {
            return;
        };
        let pane = &self.panes[ix];
        let modes = pane.snapshot.modes;
        if modes.mouse && !ev.modifiers.shift {
            let held = pane.pressed;
            if let Some(bytes) = encode_mouse(
                MouseAction::Move(held),
                held.unwrap_or(TermButton::Left),
                col,
                row,
                Self::mods(&ev.modifiers),
                modes,
            ) {
                self.send_input(ix, bytes);
            }
            return;
        }
        if pane.selecting && ev.pressed_button == Some(MouseButton::Left) {
            if let Some(t) = &pane.terminal {
                t.update_selection(row, col, right);
            }
            cx.notify();
        }
    }

    fn mouse_up(&mut self, ix: usize, ev: &MouseUpEvent, cx: &mut Context<Self>) {
        let pane = &mut self.panes[ix];
        pane.selecting = false;
        let modes = pane.snapshot.modes;
        if let Some(b) = pane.pressed.take()
            && let Some((row, col, _)) = self.cell_at(ix, ev.position)
            && let Some(bytes) = encode_mouse(
                MouseAction::Release,
                b,
                col,
                row,
                Self::mods(&ev.modifiers),
                modes,
            )
        {
            self.send_input(ix, bytes);
        }
        cx.notify();
    }

    fn wheel(&mut self, ix: usize, ev: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let lines = match ev.delta {
            ScrollDelta::Lines(p) => p.y.round() as i32,
            ScrollDelta::Pixels(p) => {
                let line_h = self
                    .panes
                    .get(ix)
                    .map(|pane| pane.geom.get().line_h)
                    .filter(|h| *h > 0.)
                    .unwrap_or(19.);
                (f32::from(p.y) / line_h).round() as i32
            }
        };
        if lines == 0 {
            return;
        }
        let modes = self.panes[ix].snapshot.modes;
        if modes.mouse && !ev.modifiers.shift {
            if let Some((row, col, _)) = self.cell_at(ix, ev.position) {
                let b = if lines > 0 {
                    TermButton::WheelUp
                } else {
                    TermButton::WheelDown
                };
                for _ in 0..lines.unsigned_abs().min(10) {
                    if let Some(bytes) = encode_mouse(
                        MouseAction::Press,
                        b,
                        col,
                        row,
                        Self::mods(&ev.modifiers),
                        modes,
                    ) {
                        self.send_input(ix, bytes);
                    }
                }
            }
        } else if modes.alt_screen && modes.alternate_scroll {
            let key = if lines > 0 { Key::Up } else { Key::Down };
            for _ in 0..lines.unsigned_abs().min(10) {
                if let Some(bytes) = encode_key(&key, Mods::default(), modes) {
                    self.send_input(ix, bytes);
                }
            }
        } else {
            self.scroll(ix, lines * 3, cx);
        }
    }

    fn render_pane(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let p = palette(cx);
        let multi = self.panes.len() > 1;
        let is_active = ix == self.active;
        let weak = cx.entity().downgrade();
        let pane = &mut self.panes[ix];
        // Renders only happen on wakeups and input, so a fresh copy is always wanted.
        if let Some(t) = &pane.terminal {
            pane.snapshot = Rc::new(t.snapshot());
        }
        let focused = pane.focus.is_focused(window);
        let snap = pane.snapshot.clone();
        let geom = pane.geom.clone();
        let term_id = pane.id;
        let focus_handle = pane.focus.clone();
        let state = pane.state.clone();
        let search_pos = snap.search;
        let pane_empty = snap.lines.iter().all(|l| l.text.trim().is_empty());
        let grid = canvas(
            move |bounds, window, cx| prepaint(bounds, &snap, &geom, term_id, weak, &p, window, cx),
            move |_bounds, frame, window, cx| paint(frame, focused, &p, window, cx),
        )
        .size_full();
        let mut body = div()
            .id(("term-pane", ix))
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(p.term)
            .overflow_hidden()
            .track_focus(&focus_handle)
            .key_context("Terminal")
            .when(multi && is_active, |d| d.border_1().border_color(p.acc))
            .on_key_down(
                cx.listener(move |this, ev: &KeyDownEvent, w, cx| this.on_key(ix, ev, w, cx)),
            )
            .on_action(cx.listener(move |this, _: &TermCopy, _, cx| this.copy(ix, cx)))
            .on_action(cx.listener(move |this, _: &TermPaste, _, cx| this.paste(ix, cx)))
            .on_action(cx.listener(|this, _: &TermFind, w, cx| this.open_search(w, cx)))
            .on_action(cx.listener(|this, _: &TermSplit, w, cx| this.split(w, cx)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev, w, cx| this.mouse_down(ix, ev, w, cx)),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, ev, w, cx| this.mouse_down(ix, ev, w, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev, w, cx| this.mouse_down(ix, ev, w, cx)),
            )
            .on_mouse_move(cx.listener(move |this, ev, _, cx| this.mouse_move(ix, ev, cx)))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, ev, _, cx| this.mouse_up(ix, ev, cx)),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(move |this, ev, _, cx| this.mouse_up(ix, ev, cx)),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(move |this, ev, _, cx| this.mouse_up(ix, ev, cx)),
            )
            .on_scroll_wheel(cx.listener(move |this, ev, _, cx| this.wheel(ix, ev, cx)))
            .child(grid);
        if is_active && let Some(search) = &self.search {
            let (pos, total) = search_pos.unwrap_or((0, 0));
            body = body.child(
                div()
                    .absolute()
                    .top(px(8.))
                    .right(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .pl(px(10.))
                    .pr(px(4.))
                    .py(px(4.))
                    .bg(p.elev)
                    .rounded(px(6.))
                    .shadow(ui::shadow(&p))
                    .text_size(px(12.))
                    .child(
                        div()
                            .w(px(170.))
                            .font_family(MONO)
                            .child(Input::new(search).appearance(false).text_size(px(12.))),
                    )
                    .child(
                        div()
                            .text_color(p.fg3)
                            .whitespace_nowrap()
                            .child(if total == 0 {
                                "no matches".to_owned()
                            } else {
                                format!("{} of {total}", total - pos)
                            }),
                    )
                    .child(
                        ui::button("ts-prev", "↑", Kind::Ghost, &p)
                            .h(px(22.))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(t) = this.active_terminal() {
                                    t.search_step(false);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        ui::button("ts-next", "↓", Kind::Ghost, &p)
                            .h(px(22.))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(t) = this.active_terminal() {
                                    t.search_step(true);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        ui::button("ts-close", "×", Kind::Ghost, &p)
                            .h(px(22.))
                            .on_click(cx.listener(|this, _, w, cx| this.close_search(w, cx))),
                    )
                    .on_key_down(cx.listener(|this, ev: &KeyDownEvent, w, cx| {
                        if ev.keystroke.key == "escape" {
                            this.close_search(w, cx);
                            cx.stop_propagation();
                        }
                    })),
            );
        }
        match state {
            PaneState::Reconnecting { .. } => body.opacity(0.55),
            PaneState::Blocked(key) => body.child(self.render_blocked(ix, &key, &p, cx)),
            PaneState::Connecting if pane_empty => body.child(
                div()
                    .absolute()
                    .top(px(PAD_Y))
                    .left(px(PAD_X))
                    .font_family(MONO)
                    .text_size(crate::appearance::editor_font_size(cx))
                    .text_color(p.fg3)
                    .child("Connecting…"),
            ),
            _ => body,
        }
        .into_any_element()
    }

    /// "Connection blocked" screen for a changed host key (design: hostKeyChanged).
    fn render_blocked(
        &self,
        ix: usize,
        key: &ChangedKey,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let file = key
            .location
            .rsplit_once(':')
            .map_or(key.location.clone(), |(f, _)| f.to_owned());
        let host_id = key.host_id.clone();
        let received = key.received.clone();
        div()
            .absolute()
            .inset_0()
            .bg(p.bg)
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(540.))
                    .border_1()
                    .border_color(p.prod)
                    .rounded(px(10.))
                    .bg(p.surface)
                    .overflow_hidden()
                    .child(
                        div()
                            .px(px(20.))
                            .py(px(14.))
                            .bg(p.prod_bg)
                            .border_b_1()
                            .border_color(p.bd)
                            .child(
                                div()
                                    .font_family(MONO)
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(px(10.5))
                                    .text_color(p.prod)
                                    .mb(px(4.))
                                    .child("CONNECTION BLOCKED"),
                            )
                            .child(
                                div()
                                    .text_size(px(15.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(format!("The host key for {} has changed", key.host)),
                            ),
                    )
                    .child(
                        div()
                            .px(px(20.))
                            .py(px(16.))
                            .flex()
                            .flex_col()
                            .gap(px(12.))
                            .child(div().text_size(px(12.5)).text_color(p.fg2).child(format!(
                                "The server at {} presented a different key than the one in {}. This can mean the server was rebuilt — or that someone is intercepting the connection.",
                                key.address, file
                            )))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(6.))
                                    .font_family(MONO)
                                    .text_size(px(12.))
                                    .child(
                                        div()
                                            .flex()
                                            .gap(px(10.))
                                            .child(div().w(px(72.)).text_color(p.fg3).child("Stored"))
                                            .child(div().line_through().text_color(p.fg2).child(key.stored.clone())),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .gap(px(10.))
                                            .child(div().w(px(72.)).text_color(p.fg3).child("Received"))
                                            .child(div().text_color(p.prod).child(key.received.clone())),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap(px(6.))
                                    .justify_end()
                                    .pt(px(4.))
                                    .child(
                                        ui::button("hk-replace", "Replace stored key…", Kind::Ghost, p)
                                            .text_color(p.prod)
                                            .on_click(cx.listener(move |this, _, w, cx| {
                                                // Trust exactly the key the user just saw, then
                                                // connect again.
                                                this.core.send(Command::AcceptChangedHostKey {
                                                    host: host_id.clone(),
                                                    fingerprint: received.clone(),
                                                });
                                                this.reconnect(ix, cx);
                                                this.refocus(w, cx);
                                            })),
                                    )
                                    .child(
                                        ui::button("hk-open", "Open known_hosts", Kind::Secondary, p)
                                            .on_click(cx.listener(move |_, _, _, cx| {
                                                cx.open_url(&format!("file://{file}"));
                                            })),
                                    )
                                    .child(
                                        ui::button("hk-stay", "Stay disconnected", Kind::Primary, p)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if let Some(p) = this.panes.get_mut(ix) {
                                                    p.state = PaneState::Failed(
                                                        "Blocked: the host key has changed".into(),
                                                    );
                                                }
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn banner(&self, p: &Palette, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        let (title, text, color, bg) = match &self.panes.get(self.active)?.state {
            PaneState::Exited(code) => (
                "Disconnected",
                match code {
                    Some(c) => format!("The program exited with code {c} · scrollback kept"),
                    None => "The program ended · scrollback kept".into(),
                },
                p.prod,
                p.prod_bg,
            ),
            PaneState::Failed(m) => ("Could not connect", m.clone(), p.prod, p.prod_bg),
            PaneState::Reconnecting {
                attempt,
                of,
                in_secs,
            } => (
                "Connection lost",
                if *in_secs == 0 {
                    format!("Reconnecting now · attempt {attempt} of {of} · scrollback kept")
                } else {
                    format!(
                        "Reconnecting in {in_secs} s · attempt {attempt} of {of} · scrollback kept"
                    )
                },
                p.stg,
                p.stg_bg,
            ),
            _ => return None,
        };
        let ix = self.active;
        let retry_now = matches!(self.panes[ix].state, PaneState::Reconnecting { .. });
        Some(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(12.))
                .py(px(7.))
                .bg(bg)
                .border_b_1()
                .border_color(p.bd)
                .text_size(px(12.))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color)
                        .child(title),
                )
                .child(div().min_w_0().truncate().child(text))
                .child(div().flex_1())
                .child(
                    ui::button("t-reconnect", "Reconnect now", Kind::Secondary, p)
                        .h(px(22.))
                        .on_click(cx.listener(move |this, _, w, cx| {
                            if retry_now {
                                // Skip the backoff; the scrollback stays.
                                let term = this.panes[ix].id;
                                this.core.send(Command::ReconnectTerminal { term });
                            } else {
                                this.reconnect(ix, cx);
                            }
                            this.refocus(w, cx);
                        })),
                )
                .into_any_element(),
        )
    }
}

impl Focusable for TerminalTab {
    fn focus_handle(&self, _cx: &gpui_kit::App) -> FocusHandle {
        self.panes[self.active.min(self.panes.len().saturating_sub(1))]
            .focus
            .clone()
    }
}

/// Everything `paint` needs, computed in `prepaint`.
struct Frame {
    origin: Point<Pixels>,
    cell_w: f32,
    line_h: f32,
    bgs: Vec<(Bounds<Pixels>, Hsla)>,
    lines: Vec<(Point<Pixels>, gpui_kit::ShapedLine)>,
    cursor: Option<(Bounds<Pixels>, CursorShape, Option<gpui_kit::ShapedLine>)>,
    cursor_color: Hsla,
}

fn color(c: TermColor, p: &Palette, fg: bool) -> Hsla {
    match c {
        TermColor::Foreground => p.fg,
        TermColor::Background => {
            if fg {
                p.term
            } else {
                gpui_kit::transparent_black()
            }
        }
        TermColor::Indexed(i) => p.ansi(i),
        TermColor::Rgb(r, g, b) => gpui_kit::Rgba {
            r: f32::from(r) / 255.,
            g: f32::from(g) / 255.,
            b: f32::from(b) / 255.,
            a: 1.,
        }
        .into(),
    }
}

fn mono(family: &SharedString, bold: bool, italic: bool) -> Font {
    let mut f = font(family.clone());
    if bold {
        f.weight = FontWeight::SEMIBOLD;
    }
    if italic {
        f.style = FontStyle::Italic;
    }
    f
}

#[allow(clippy::too_many_arguments)]
fn prepaint(
    bounds: Bounds<Pixels>,
    snap: &Snapshot,
    geom: &Rc<Cell<Geom>>,
    term: TermId,
    view: WeakEntity<TerminalTab>,
    p: &Palette,
    window: &mut Window,
    cx: &mut gpui_kit::App,
) -> Frame {
    let metrics = crate::appearance::term_metrics(cx);
    let family = metrics.family;
    let line_h = metrics.line_height;
    let font_size = px(metrics.font_size);
    let ts = window.text_system().clone();
    let font_id = ts.resolve_font(&mono(&family, false, false));
    let cell_w = ts
        .advance(font_id, font_size, 'm')
        .map(|s| f32::from(s.width))
        .unwrap_or(metrics.font_size * 0.6)
        .max(1.);
    let origin = point(bounds.origin.x + px(PAD_X), bounds.origin.y + px(PAD_Y));
    let avail_w = f32::from(bounds.size.width) - 2. * PAD_X;
    let avail_h = f32::from(bounds.size.height) - 2. * PAD_Y;
    let cols = ((avail_w / cell_w).floor() as u16).max(2);
    let rows = ((avail_h / line_h).floor() as u16).max(1);
    geom.set(Geom {
        origin,
        cell_w,
        line_h,
        cols,
        rows,
    });
    if (cols != snap.cols || rows != snap.rows) && snap.cols > 0 {
        let size = TermSize { cols, rows };
        cx.defer(move |cx| {
            let _ = view.update(cx, |tab, cx| tab.resize_pane(term, size, cx));
        });
    }

    let mut bgs = Vec::new();
    let mut lines = Vec::new();
    for (row, line) in snap.lines.iter().enumerate() {
        let y = origin.y + px(row as f32 * line_h);
        let mut col = 0u16;
        let mut runs = Vec::with_capacity(line.runs.len());
        for r in &line.runs {
            let bg = match r.mark {
                Mark::Selected => Some(p.sel),
                Mark::Match => Some(p.staged),
                Mark::FocusedMatch => Some({
                    let mut c = p.stg;
                    c.a = 0.55;
                    c
                }),
                Mark::None => (r.bg != TermColor::Background).then(|| color(r.bg, p, false)),
            };
            if let Some(bg) = bg {
                bgs.push((
                    Bounds::new(
                        point(origin.x + px(f32::from(col) * cell_w), y),
                        size(px(f32::from(r.cells) * cell_w), px(line_h)),
                    ),
                    bg,
                ));
            }
            let mut fg = color(r.fg, p, true);
            if r.attrs.dim {
                fg.a *= 0.6;
            }
            runs.push(TextRun {
                len: r.len,
                font: mono(&family, r.attrs.bold, r.attrs.italic),
                color: fg,
                background_color: None,
                underline: r.attrs.underline.then(|| UnderlineStyle {
                    thickness: px(1.),
                    color: Some(fg),
                    wavy: false,
                }),
                strikethrough: r.attrs.strike.then(|| StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(fg),
                }),
            });
            col += r.cells;
        }
        let text = line.text.trim_end_matches(' ');
        if text.is_empty() {
            continue;
        }
        // Trailing spaces were trimmed; shorten the last runs to match.
        let mut remaining = text.len();
        let mut trimmed = Vec::with_capacity(runs.len());
        for mut r in runs {
            if remaining == 0 {
                break;
            }
            r.len = r.len.min(remaining);
            remaining -= r.len;
            trimmed.push(r);
        }
        let shaped = ts.shape_line(
            SharedString::from(text.to_owned()),
            font_size,
            &trimmed,
            Some(px(cell_w)),
        );
        lines.push((point(origin.x, y), shaped));
    }

    let cursor = snap.cursor.map(|c| {
        let b = Bounds::new(
            point(
                origin.x + px(f32::from(c.col) * cell_w),
                origin.y + px(f32::from(c.row) * line_h),
            ),
            size(px(cell_w), px(line_h)),
        );
        // The glyph under a block cursor is redrawn in the background color.
        let glyph = snap
            .lines
            .get(c.row as usize)
            .and_then(|l| l.text.chars().nth(c.col as usize))
            .filter(|ch| !ch.is_whitespace())
            .map(|ch| {
                let s = ch.to_string();
                let len = s.len();
                ts.shape_line(
                    s.into(),
                    font_size,
                    &[TextRun {
                        len,
                        font: mono(&family, false, false),
                        color: p.term,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    Some(px(cell_w)),
                )
            });
        (b, c.shape, glyph)
    });

    Frame {
        origin,
        cell_w,
        line_h,
        bgs,
        lines,
        cursor,
        cursor_color: p.fg,
    }
}

fn paint(frame: Frame, focused: bool, _p: &Palette, window: &mut Window, cx: &mut gpui_kit::App) {
    let _ = (frame.origin, frame.cell_w);
    for (b, c) in &frame.bgs {
        window.paint_quad(fill(*b, *c));
    }
    let lh = px(frame.line_h);
    for (origin, line) in &frame.lines {
        let _ = line.paint(*origin, lh, TextAlign::Left, None, window, cx);
    }
    if let Some((b, shape, glyph)) = frame.cursor {
        let c = frame.cursor_color;
        match (shape, focused) {
            (CursorShape::Block, true) => {
                window.paint_quad(fill(b, c));
                if let Some(g) = glyph {
                    let _ = g.paint(b.origin, lh, TextAlign::Left, None, window, cx);
                }
            }
            (CursorShape::Beam, true) => {
                window.paint_quad(fill(Bounds::new(b.origin, size(px(2.), b.size.height)), c));
            }
            (CursorShape::Underline, true) => {
                window.paint_quad(fill(
                    Bounds::new(
                        point(b.origin.x, b.origin.y + b.size.height - px(2.)),
                        size(b.size.width, px(2.)),
                    ),
                    c,
                ));
            }
            _ => window.paint_quad(outline(b, c, gpui_kit::BorderStyle::Solid)),
        }
    }
}

impl Render for TerminalTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let pane = self.panes.get(self.active);
        let description = pane.map(|p| p.description.clone()).unwrap_or_default();
        let title = pane
            .map(|p| p.title.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| self.title.to_string());
        let (dot, label) = match pane.map(|p| &p.state) {
            Some(PaneState::Live) => (p.dev, "Connected".to_owned()),
            Some(PaneState::Connecting) => (p.stg, "Connecting".to_owned()),
            Some(PaneState::Reconnecting { .. }) => (p.stg, "Reconnecting".to_owned()),
            Some(PaneState::Exited(_)) => (p.prod, "Disconnected".to_owned()),
            Some(PaneState::Failed(_)) => (p.prod, "Failed".to_owned()),
            Some(PaneState::Blocked(_)) => (p.prod, "Blocked".to_owned()),
            None => (p.fg3, String::new()),
        };
        let panes: Vec<gpui_kit::AnyElement> = (0..self.panes.len())
            .map(|i| self.render_pane(i, window, cx))
            .collect();
        let n = panes.len();
        let can_split = n < MAX_PANES;
        let banner = self.banner(&p, cx);
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(36.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(10.))
                    .border_b_1()
                    .border_color(p.bd)
                    .text_size(px(12.))
                    .child(ui::dot(p.env(self.env), 7.))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .whitespace_nowrap()
                            .child(title),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(MONO)
                            .text_color(p.fg3)
                            .child(description),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_color(p.fg2)
                            .whitespace_nowrap()
                            .child(ui::dot(dot, 6.))
                            .child(label),
                    )
                    .child(ui::vdivider(&p, 16.))
                    .child(
                        ui::button("t-split", "Split", Kind::Ghost, &p)
                            .h(px(24.))
                            .when(!can_split, |b| b.opacity(0.4))
                            .on_click(cx.listener(|this, _, w, cx| this.split(w, cx))),
                    )
                    .when(n > 1, |d| {
                        d.child(
                            ui::button("t-close-pane", "Close pane", Kind::Ghost, &p)
                                .h(px(24.))
                                .on_click(cx.listener(|this, _, w, cx| {
                                    let ix = this.active;
                                    this.close_pane(ix, cx);
                                    this.refocus(w, cx);
                                })),
                        )
                    })
                    .child(
                        ui::button(
                            "t-bcast",
                            if self.broadcast {
                                "Broadcast: on"
                            } else {
                                "Broadcast: off"
                            },
                            Kind::Ghost,
                            &p,
                        )
                        .h(px(24.))
                        .when(self.broadcast, |b| b.text_color(p.stg))
                        .on_click(cx.listener(|this, _, w, cx| {
                            this.broadcast = !this.broadcast;
                            this.refocus(w, cx);
                            cx.notify();
                        })),
                    )
                    .child(
                        ui::button("t-find", "Find", Kind::Ghost, &p)
                            .h(px(24.))
                            .on_click(cx.listener(|this, _, w, cx| this.open_search(w, cx))),
                    ),
            )
            .when_some(banner, |d, b| d.child(b))
            .when(self.broadcast && n > 1, |d| {
                d.child(
                    div()
                        .flex_none()
                        .px(px(12.))
                        .py(px(5.))
                        .bg(p.stg_bg)
                        .border_b_1()
                        .border_color(p.bd)
                        .text_size(px(11.5))
                        .text_color(p.fg2)
                        .child(format!("Typing goes to all {n} panes")),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .gap(px(1.))
                    .bg(p.bd)
                    .children(panes),
            )
    }
}
