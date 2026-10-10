//! Open anything (⌘P) and the command palette (⇧⌘P), with fuzzy matching. One list
//! of connections, tools and commands, narrowed by chips (Tab cycles them) or by a
//! leading `>` for commands (design v3).

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, FontWeight,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
};
use switchyard_core::store::{Profile, ProfileId};

use crate::actions::{CommandId, fuzzy_score, palette_commands};
use crate::app_state::{Profiles, badge_of, describe};
use crate::appearance::{rpx, ts};
use crate::theme::{MONO, palette};
use crate::ui;

/// What the palette searches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteMode {
    /// Commands.
    Commands,
    /// Saved connections.
    Connections,
    /// Connections, tools and commands together.
    Anything,
}

/// The palette's filter chips.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chip {
    /// Everything.
    All,
    /// Saved connections.
    Connections,
    /// Tools (see [`crate::rail::tool_groups`]).
    Tools,
    /// Commands.
    Commands,
}

impl Chip {
    const ALL: [Chip; 4] = [Chip::All, Chip::Connections, Chip::Tools, Chip::Commands];

    fn label(self) -> &'static str {
        match self {
            Chip::All => "All",
            Chip::Connections => "Connections",
            Chip::Tools => "Tools",
            Chip::Commands => "Commands",
        }
    }

    fn next(self) -> Chip {
        let i = Chip::ALL.iter().position(|c| *c == self).unwrap_or(0);
        Chip::ALL[(i + 1) % Chip::ALL.len()]
    }
}

/// The chip in effect: a leading `>` means commands. Returns it and the query to match.
pub fn effective(chip: Chip, query: &str) -> (Chip, &str) {
    match query.strip_prefix('>') {
        Some(rest) => (Chip::Commands, rest.trim()),
        None => (chip, query.trim()),
    }
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
    badge: SharedString,
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
    chip: Chip,
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
            chip: match mode {
                PaletteMode::Commands => Chip::Commands,
                PaletteMode::Connections => Chip::Connections,
                PaletteMode::Anything => Chip::All,
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
        let (chip, query) = effective(self.chip, query);
        let want = |c: Chip| chip == Chip::All || chip == c;
        let mut scored: Vec<(usize, Item)> = Vec::new();
        if want(Chip::Connections) {
            scored.extend(
                self.profiles
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
                                Profile::Cloud(c) => c.service.display_name().to_owned(),
                            };
                            (
                                s,
                                Item {
                                    badge: badge_of(prof).into(),
                                    label: prof.name().to_owned().into(),
                                    group: format!("{kind} · {}", describe(prof, &self.profiles))
                                        .into(),
                                    key: "".into(),
                                    dot: Some(p.env(prof.environment())),
                                    target: Target::Profile(prof.id().clone()),
                                },
                            )
                        })
                    }),
            );
        }
        if want(Chip::Tools) {
            for g in crate::rail::tool_groups(&self.profiles) {
                for t in g.tools {
                    if let Some(s) = fuzzy_score(query, t.name) {
                        scored.push((
                            s,
                            Item {
                                badge: t.badge.into(),
                                label: t.name.into(),
                                group: g.name.into(),
                                key: t.tag.into(),
                                dot: None,
                                target: Target::Command(t.cmd),
                            },
                        ));
                    }
                }
            }
        }
        if want(Chip::Commands) {
            scored.extend(palette_commands().into_iter().filter_map(|c| {
                fuzzy_score(query, &c.label).map(|s| {
                    (
                        s,
                        Item {
                            badge: ">".into(),
                            label: c.label,
                            group: c.group.into(),
                            key: c.key,
                            dot: None,
                            target: Target::Command(c.id),
                        },
                    )
                })
            }));
        }
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

    fn set_chip(&mut self, chip: Chip, window: &mut Window, cx: &mut Context<Self>) {
        self.chip = chip;
        self.selected = 0;
        // A `>` prefix would override the chip.
        if self.input.read(cx).value().starts_with('>') {
            self.input.update(cx, |i, cx| i.set_value("", window, cx));
        }
        cx.notify();
    }
}

fn placeholder(mode: PaletteMode) -> &'static str {
    match mode {
        PaletteMode::Commands => "Type a command…",
        PaletteMode::Anything => "Open anything…  type > for commands",
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
        let (chip, _) = effective(self.chip, &q);
        let rows: Vec<AnyElement> = items
            .into_iter()
            .enumerate()
            .map(|(i, it)| {
                let sel = i == self.selected;
                div()
                    .id(("pal-item", i))
                    .h(rpx(32.))
                    .flex()
                    .items_center()
                    .gap(rpx(10.))
                    .px(rpx(10.))
                    .rounded(px(6.))
                    .text_size(ts::BASE)
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
                    .child(ui::monogram(it.badge, 28., &p))
                    .child(div().flex_1().min_w_0().truncate().child(it.label))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(rpx(6.))
                            .text_size(ts::LABEL)
                            .text_color(p.fg3)
                            .whitespace_nowrap()
                            .when_some(it.dot, |d, c| d.child(ui::dot(c, 6.)))
                            .child(it.group),
                    )
                    .when(!it.key.is_empty(), |d| {
                        d.child(
                            div()
                                .min_w(rpx(44.))
                                .flex()
                                .justify_end()
                                .font_family(MONO)
                                .text_size(ts::CAPTION_PLUS)
                                .text_color(p.fg2)
                                .child(it.key),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        let chips = Chip::ALL.into_iter().map(|c| {
            let on = c == chip;
            div()
                .id(c.label())
                .h(rpx(22.))
                .px(rpx(9.))
                .flex()
                .items_center()
                .border_1()
                .border_color(if on { p.acc } else { p.bd2 })
                .rounded(px(11.))
                .when(on, |d| d.bg(p.sel))
                .text_size(ts::LABEL)
                .font_weight(FontWeight::MEDIUM)
                .text_color(if on { p.fg } else { p.fg2 })
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, w, cx| this.set_chip(c, w, cx)))
                .child(c.label())
        });
        div()
            .id("palette")
            .w(rpx(640.))
            .bg(p.elev)
            .rounded(px(10.))
            .shadow(ui::shadow(&p))
            .overflow_hidden()
            .capture_key_down(cx.listener(move |this, ev: &KeyDownEvent, w, cx| {
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
                    "tab" => {
                        let q = this.input.read(cx).value().to_string();
                        let next = effective(this.chip, &q).0.next();
                        this.set_chip(next, w, cx);
                        cx.stop_propagation();
                    }
                    _ => {}
                }
            }))
            .child(
                div()
                    .h(rpx(46.))
                    .flex()
                    .items_center()
                    .px(rpx(14.))
                    .border_b_1()
                    .border_color(p.bd)
                    .child(
                        div().flex_1().child(
                            Input::new(&self.input)
                                .appearance(false)
                                .text_size(ts::TITLE),
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap(rpx(6.))
                    .px(rpx(12.))
                    .py(rpx(8.))
                    .border_b_1()
                    .border_color(p.bd)
                    .children(chips),
            )
            .child(
                div()
                    .id("pal-list")
                    .max_h(rpx(360.))
                    .overflow_y_scroll()
                    .p(rpx(4.))
                    .children(rows)
                    .when(n == 0, |d| {
                        d.child(
                            div()
                                .py(rpx(28.))
                                .px(rpx(16.))
                                .flex()
                                .flex_col()
                                .items_center()
                                .text_color(p.fg2)
                                .text_size(ts::BASE)
                                .child(format!("No matches for “{q}”"))
                                .child(
                                    div()
                                        .mt(rpx(4.))
                                        .text_color(p.fg3)
                                        .text_size(ts::BODY)
                                        .child("Tab switches the filter; > searches commands"),
                                ),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap(rpx(14.))
                    .px(rpx(14.))
                    .py(rpx(8.))
                    .border_t_1()
                    .border_color(p.bd)
                    .text_size(ts::SMALL)
                    .text_color(p.fg3)
                    .child("↑↓ navigate")
                    .child("↵ open")
                    .child("tab next filter")
                    .child("esc close")
                    .child(div().flex_1())
                    .child(format!("{n} results")),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_leading_angle_bracket_searches_commands() {
        assert_eq!(effective(Chip::All, "> run"), (Chip::Commands, "run"));
        assert_eq!(effective(Chip::Tools, " s3 "), (Chip::Tools, "s3"));
    }

    #[test]
    fn tab_cycles_the_chips() {
        let mut c = Chip::All;
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(c);
            c = c.next();
        }
        assert_eq!(seen, Chip::ALL);
        assert_eq!(c, Chip::All);
    }
}
