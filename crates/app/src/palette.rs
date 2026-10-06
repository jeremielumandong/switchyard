//! Command palette (⇧⌘P) and quick switcher (⌘P) with fuzzy matching.

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
};
use switchyard_core::store::{Profile, ProfileId};

use crate::actions::{CommandId, fuzzy_score, palette_commands};
use crate::app_state::{Profiles, describe};
use crate::theme::{MONO, palette};
use crate::ui;

/// What the palette searches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteMode {
    /// Commands.
    Commands,
    /// Saved connections.
    Connections,
    /// Schema objects (opens connections list filtered; objects come from the sidebar).
    Objects,
}

/// What the user picked.
pub enum PaletteEvent {
    /// Run a command.
    Run(CommandId),
    /// Open a profile.
    Open(ProfileId),
    /// Closed without a choice.
    #[allow(dead_code)]
    Dismiss,
}

struct Item {
    label: SharedString,
    group: SharedString,
    key: SharedString,
    dot: Option<gpui_kit::Hsla>,
    target: Target,
}

enum Target {
    Command(CommandId),
    Profile(ProfileId),
}

/// The palette view.
pub struct PaletteView {
    mode: PaletteMode,
    input: Entity<InputState>,
    selected: usize,
    profiles: Profiles,
    _sub: Subscription,
}

impl EventEmitter<PaletteEvent> for PaletteView {}

impl PaletteView {
    /// A palette in `mode`.
    pub fn new(
        mode: PaletteMode,
        profiles: Profiles,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder(mode)));
        input.update(cx, |i, cx| i.focus(window, cx));
        let sub = cx.subscribe(&input, |this, _, ev: &InputEvent, cx| match ev {
            InputEvent::Change => {
                this.selected = 0;
                cx.notify();
            }
            InputEvent::PressEnter { .. } => this.confirm(cx),
            _ => {}
        });
        Self {
            mode: if mode == PaletteMode::Objects {
                PaletteMode::Connections
            } else {
                mode
            },
            input,
            selected: 0,
            profiles,
            _sub: sub,
        }
    }

    /// Update the profile list while open.
    pub fn set_profiles(&mut self, profiles: Profiles, cx: &mut Context<Self>) {
        self.profiles = profiles;
        cx.notify();
    }

    fn items(&self, query: &str, cx: &Context<Self>) -> Vec<Item> {
        let p = palette(cx);
        let mut scored: Vec<(usize, Item)> = match self.mode {
            PaletteMode::Commands => palette_commands()
                .into_iter()
                .filter_map(|c| {
                    fuzzy_score(query, &c.label).map(|s| {
                        (
                            s,
                            Item {
                                label: c.label,
                                group: c.group.into(),
                                key: c.key,
                                dot: None,
                                target: Target::Command(c.id),
                            },
                        )
                    })
                })
                .collect(),
            _ => self
                .profiles
                .all
                .iter()
                .filter(|p| !matches!(p, Profile::Terminal(_)))
                .filter_map(|prof| {
                    fuzzy_score(query, prof.name()).map(|s| {
                        let kind = match prof {
                            Profile::Db(d) => d.engine.display_name().to_owned(),
                            Profile::Host(_) => "SSH".into(),
                            Profile::File(_) => "Files".into(),
                            Profile::Terminal(_) => "Terminal".into(),
                        };
                        (
                            s,
                            Item {
                                label: prof.name().to_owned().into(),
                                group: format!("{kind} · {}", describe(prof, &self.profiles))
                                    .into(),
                                key: "".into(),
                                dot: Some(p.env(prof.environment())),
                                target: Target::Profile(prof.id().clone()),
                            },
                        )
                    })
                })
                .collect(),
        };
        if !query.is_empty() {
            scored.sort_by_key(|(s, _)| *s);
        }
        scored.into_iter().map(|(_, i)| i).collect()
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        let q = self.input.read(cx).value().to_string();
        let items = self.items(&q, cx);
        if let Some(item) = items.into_iter().nth(self.selected) {
            match item.target {
                Target::Command(id) => cx.emit(PaletteEvent::Run(id)),
                Target::Profile(id) => cx.emit(PaletteEvent::Open(id)),
            }
        }
    }

    fn swap(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = match self.mode {
            PaletteMode::Commands => PaletteMode::Connections,
            _ => PaletteMode::Commands,
        };
        self.selected = 0;
        let ph = placeholder(self.mode);
        self.input.update(cx, |i, cx| {
            i.set_value("", window, cx);
            i.set_placeholder(ph, window, cx);
        });
        cx.notify();
    }
}

fn placeholder(mode: PaletteMode) -> &'static str {
    match mode {
        PaletteMode::Commands => "Type a command…",
        _ => "Switch to a connection…",
    }
}

impl Render for PaletteView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let q = self.input.read(cx).value().to_string();
        let items = self.items(&q, cx);
        let n = items.len();
        self.selected = self.selected.min(n.saturating_sub(1));
        let cmd = self.mode == PaletteMode::Commands;
        let (mode_label, swap_label, noun) = if cmd {
            (
                "COMMANDS",
                format!("{} connections", ui::keys("⌘P", "Ctrl+P")),
                "commands",
            )
        } else {
            (
                "CONNECTIONS",
                format!("{} commands", ui::keys("⇧⌘P", "Ctrl+Shift+P")),
                "connections",
            )
        };
        let rows: Vec<AnyElement> = items
            .into_iter()
            .enumerate()
            .map(|(i, it)| {
                let sel = i == self.selected;
                div()
                    .id(("pal-item", i))
                    .h(px(32.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(10.))
                    .rounded(px(6.))
                    .text_size(px(13.))
                    .when(sel, |d| d.bg(p.sel))
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        if this.selected != i {
                            this.selected = i;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.selected = i;
                        this.confirm(cx);
                    }))
                    .child(ui::dot(it.dot.unwrap_or(gpui_kit::transparent_black()), 7.))
                    .child(div().flex_1().min_w_0().truncate().child(it.label))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(p.fg3)
                            .whitespace_nowrap()
                            .child(it.group),
                    )
                    .child(
                        div()
                            .min_w(px(44.))
                            .flex()
                            .justify_end()
                            .font_family(MONO)
                            .text_size(px(10.5))
                            .text_color(p.fg2)
                            .child(it.key),
                    )
                    .into_any_element()
            })
            .collect();
        div()
            .id("palette")
            .w(px(620.))
            .bg(p.elev)
            .rounded(px(10.))
            .shadow(ui::shadow(&p))
            .overflow_hidden()
            .capture_key_down(cx.listener(move |this, ev: &KeyDownEvent, _, cx| {
                match ev.keystroke.key.as_str() {
                    "down" => {
                        this.selected = (this.selected + 1).min(n.saturating_sub(1));
                        cx.stop_propagation();
                        cx.notify();
                    }
                    "up" => {
                        this.selected = this.selected.saturating_sub(1);
                        cx.stop_propagation();
                        cx.notify();
                    }
                    _ => {}
                }
            }))
            .child(
                div()
                    .h(px(46.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(14.))
                    .border_b_1()
                    .border_color(p.bd)
                    .child(
                        div()
                            .flex_none()
                            .px(px(6.))
                            .py(px(2.))
                            .rounded(px(4.))
                            .bg(p.hover)
                            .text_color(p.fg2)
                            .font_family(MONO)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_size(px(10.))
                            .child(mode_label),
                    )
                    .child(div().flex_1().child(Input::new(&self.input).appearance(false).text_size(px(14.))))
                    .child(
                        div()
                            .id("pal-swap")
                            .flex_none()
                            .font_family(MONO)
                            .text_size(px(10.5))
                            .text_color(p.fg3)
                            .whitespace_nowrap()
                            .on_click(cx.listener(|this, _, w, cx| this.swap(w, cx)))
                            .child(swap_label),
                    ),
            )
            .child(
                div()
                    .id("pal-list")
                    .max_h(px(380.))
                    .overflow_y_scroll()
                    .p(px(4.))
                    .children(rows)
                    .when(n == 0, |d| {
                        d.child(
                            div()
                                .py(px(28.))
                                .px(px(16.))
                                .flex()
                                .flex_col()
                                .items_center()
                                .text_color(p.fg2)
                                .text_size(px(13.))
                                .child(format!("No matches for “{q}”"))
                                .child(div().mt(px(4.)).text_color(p.fg3).text_size(px(12.)).child(if cmd {
                                    format!("Try “run” or “settings”, or {} to search connections", ui::keys("⌘P", "Ctrl+P"))
                                } else {
                                    format!("Press {} to search commands instead", ui::keys("⇧⌘P", "Ctrl+Shift+P"))
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap(px(14.))
                    .px(px(14.))
                    .py(px(8.))
                    .border_t_1()
                    .border_color(p.bd)
                    .text_size(px(11.))
                    .text_color(p.fg3)
                    .child("↑↓ navigate")
                    .child("↵ run")
                    .child("esc close")
                    .child(div().flex_1())
                    .child(format!("{n} {noun}")),
            )
    }
}
