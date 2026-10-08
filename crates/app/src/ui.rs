//! Small presentation helpers that mirror the design's component vocabulary: buttons,
//! monogram badges, keyboard hints, environment dots and badges, segmented controls.

use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, BoxShadow, ClickEvent, Div, ElementId, FontWeight, Hsla, InteractiveElement,
    IntoElement, ParentElement, SharedString, Stateful, StatefulInteractiveElement, Styled, Window,
    div, point, px,
};
use switchyard_core::store::EnvironmentLabel;

use crate::theme::{MONO, Palette, SANS};

/// Button style variants from the design.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Accent fill.
    Primary,
    /// Bordered on the elevated surface.
    Secondary,
    /// No border or fill.
    Ghost,
    /// Production-red fill.
    Destructive,
}

/// Click handler type.
pub type OnClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// A design-system button.
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    kind: Kind,
    p: &Palette,
) -> Stateful<Div> {
    let (bg, fg, border, weight) = match kind {
        Kind::Primary => (p.acc, p.acc_fg, None, FontWeight::SEMIBOLD),
        Kind::Secondary => (p.elev, p.fg, Some(p.bd2), FontWeight::MEDIUM),
        Kind::Ghost => (
            gpui_kit::transparent_black(),
            p.fg2,
            None,
            FontWeight::MEDIUM,
        ),
        Kind::Destructive => (p.prod, gpui_kit::white(), None, FontWeight::SEMIBOLD),
    };
    let hover = p.hover;
    let fg_hover = p.fg;
    let bd3 = p.fg3;
    div()
        .id(id.into())
        .h(px(26.))
        .px(px(10.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .gap(px(8.))
        .rounded(px(6.))
        .bg(bg)
        .text_color(fg)
        .font_family(SANS)
        .text_size(px(12.))
        .font_weight(weight)
        .whitespace_nowrap()
        .when_some(border, |d, b| d.border_1().border_color(b))
        .when(kind == Kind::Ghost, move |d| {
            d.hover(move |s| s.bg(hover).text_color(fg_hover))
        })
        .when(kind == Kind::Secondary, move |d| {
            d.hover(move |s| s.border_color(bd3))
        })
        .child(label.into())
}

/// A button with a trailing keyboard hint, like "Run ⌘↵".
pub fn button_with_key(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    key: impl Into<SharedString>,
    kind: Kind,
    p: &Palette,
) -> Stateful<Div> {
    let key_color = if kind == Kind::Primary {
        p.acc_fg.opacity(0.7)
    } else {
        p.fg3
    };
    button(id, label, kind, p).pl(px(10.)).pr(px(8.)).child(
        mono(key, px(10.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(key_color),
    )
}

/// A compact square icon button (ghost style) for toolbars and the title bar. `active`
/// keeps it highlighted, e.g. while the panel it toggles is open. Callers add the
/// tooltip, since that is where the action name and shortcut are known.
pub fn icon_button(
    id: impl Into<ElementId>,
    icon: IconName,
    active: bool,
    p: &Palette,
) -> Stateful<Div> {
    let hover = p.hover;
    let fg_hover = p.fg;
    div()
        .id(id.into())
        .size(px(26.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .cursor_pointer()
        .text_color(if active { p.acc } else { p.fg2 })
        .when(active, |d| d.bg(p.sel))
        .hover(move |s| s.bg(hover).text_color(fg_hover))
        .child(Icon::new(icon).size(px(15.)))
}

/// Monospace text.
pub fn mono(text: impl Into<SharedString>, size: gpui_kit::Pixels) -> Div {
    div().font_family(MONO).text_size(size).child(text.into())
}

/// A keyboard shortcut chip.
pub fn kbd(text: impl Into<SharedString>, p: &Palette) -> Div {
    div()
        .flex_none()
        .px(px(5.))
        .py(px(1.))
        .rounded(px(4.))
        .border_1()
        .border_color(p.bd2)
        .text_color(p.fg2)
        .font_family(MONO)
        .font_weight(FontWeight::MEDIUM)
        .text_size(px(10.5))
        .child(text.into())
}

/// A connection-type monogram (`PG`, `SSH`, ...).
pub fn monogram(text: impl Into<SharedString>, width: f32, p: &Palette) -> Div {
    div()
        .flex_none()
        .w(px(width))
        .h(px(15.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(3.))
        .bg(p.hover)
        .border_1()
        .border_color(p.bd)
        .text_color(p.fg2)
        .font_family(MONO)
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(8.5))
        .child(text.into())
}

/// A small round dot.
pub fn dot(color: Hsla, size: f32) -> Div {
    div().flex_none().size(px(size)).rounded_full().bg(color)
}

/// An outlined environment badge (`PROD`).
pub fn env_badge(env: EnvironmentLabel, p: &Palette) -> Div {
    let c = p.env(env);
    div()
        .flex_none()
        .px(px(6.))
        .py(px(2.))
        .rounded(px(4.))
        .border_1()
        .border_color(c)
        .text_color(c)
        .font_family(MONO)
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(10.))
        .child(env.badge())
}

/// A filled environment badge (`PRODUCTION`).
pub fn env_badge_solid(env: EnvironmentLabel, p: &Palette) -> Div {
    div()
        .flex_none()
        .px(px(7.))
        .py(px(3.))
        .rounded(px(4.))
        .bg(p.env(env))
        .text_color(p.env_on(env))
        .font_family(MONO)
        .font_weight(FontWeight::SEMIBOLD)
        .text_size(px(10.))
        .child(env.name().to_uppercase())
}

/// The design's popover / dialog shadow.
pub fn shadow(p: &Palette) -> Vec<BoxShadow> {
    vec![
        BoxShadow {
            color: p.shadow,
            offset: point(px(0.), px(16.)),
            blur_radius: px(48.),
            spread_radius: px(0.),
            inset: false,
        },
        BoxShadow {
            color: if p.dark {
                gpui_kit::hsla(0., 0., 1., 0.07)
            } else {
                gpui_kit::hsla(0., 0., 0., 0.07)
            },
            offset: point(px(0.), px(0.)),
            blur_radius: px(0.),
            spread_radius: px(1.),
            inset: false,
        },
    ]
}

/// A segmented control (two or more options in a pill).
pub fn segmented(
    id: impl Into<ElementId>,
    options: Vec<(SharedString, bool, OnClick)>,
    height: f32,
    p: &Palette,
) -> Stateful<Div> {
    let id: ElementId = id.into();
    div()
        .id(id)
        .flex()
        .flex_none()
        .p(px(2.))
        .gap(px(2.))
        .bg(p.bg)
        .border_1()
        .border_color(p.bd)
        .rounded(px(6.))
        .children(
            options
                .into_iter()
                .enumerate()
                .map(|(i, (label, active, on_click))| {
                    div()
                        .id(("seg", i))
                        .flex_1()
                        .h(px(height))
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.))
                        .font_family(SANS)
                        .font_weight(FontWeight::MEDIUM)
                        .text_size(px(if height <= 20. { 11.5 } else { 12. }))
                        .whitespace_nowrap()
                        .when(active, |d| d.bg(p.elev).text_color(p.fg))
                        .when(!active, |d| d.text_color(p.fg2))
                        .on_click(on_click)
                        .child(label)
                        .into_any_element()
                }),
        )
}

/// A checkbox with a label.
pub fn checkbox(
    id: impl Into<ElementId>,
    checked: bool,
    label: impl Into<SharedString>,
    p: &Palette,
) -> Stateful<Div> {
    div()
        .id(id.into())
        .flex()
        .items_center()
        .gap(px(8.))
        .text_size(px(12.))
        .text_color(p.fg2)
        .child(
            div()
                .size(px(14.))
                .flex()
                .items_center()
                .justify_center()
                .border_1()
                .border_color(if checked { p.acc } else { p.bd2 })
                .rounded(px(3.))
                .bg(if checked { p.acc } else { p.surface })
                .text_color(p.acc_fg)
                .text_size(px(10.))
                .child(if checked { "✓" } else { "" }),
        )
        .child(label.into())
}

/// Section caption in small caps style ("HOSTS", "RECENT").
pub fn caption(text: impl Into<SharedString>, p: &Palette) -> Div {
    div()
        .text_color(p.fg3)
        .text_size(px(11.))
        .font_weight(FontWeight::MEDIUM)
        .child(text.into())
}

/// A vertical divider used in toolbars.
pub fn vdivider(p: &Palette, height: f32) -> Div {
    div()
        .flex_none()
        .w(px(1.))
        .h(px(height))
        .bg(p.bd)
        .mx(px(4.))
}

/// A shimmering placeholder bar (loading states).
pub fn shimmer(width: f32, p: &Palette) -> AnyElement {
    use gpui_kit::{Animation, AnimationExt as _};
    let base = p.hover;
    let peak = p.bd2;
    div()
        .flex_none()
        .w(px(width))
        .h(px(8.))
        .rounded(px(4.))
        .bg(base)
        .with_animation(
            ElementId::Name(format!("shimmer-{width}").into()),
            Animation::new(std::time::Duration::from_millis(1200)).repeat(),
            move |d, t| {
                let k = 1.0 - (t * 2.0 - 1.0).abs();
                d.bg(gpui_kit::Hsla {
                    h: peak.h,
                    s: peak.s,
                    l: base.l + (peak.l - base.l) * k,
                    a: base.a + (peak.a - base.a) * k,
                })
            },
        )
        .into_any_element()
}

/// A pulsing status dot (running / connecting).
pub fn pulse_dot(id: impl Into<ElementId>, color: Hsla, size: f32) -> AnyElement {
    use gpui_kit::{Animation, AnimationExt as _};
    dot(color, size)
        .with_animation(
            id.into(),
            Animation::new(std::time::Duration::from_millis(1000)).repeat(),
            |d, t| d.opacity(0.3 + 0.7 * (1.0 - (t * 2.0 - 1.0).abs())),
        )
        .into_any_element()
}

/// Format an integer with thousands separators.
pub fn thousands(n: impl Into<u128>) -> String {
    let s = n.into().to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Human duration: `84 ms`, `2.4 s`, `1 min 3 s`.
pub fn duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{} min {} s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// Human byte size.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Platform-specific shortcut label.
pub fn keys(mac: &str, other: &str) -> SharedString {
    if cfg!(target_os = "macos") {
        mac.to_owned().into()
    } else {
        other.to_owned().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(thousands(1_248u32), "1,248");
        assert_eq!(thousands(1_000_000u32), "1,000,000");
        assert_eq!(thousands(12u32), "12");
        assert_eq!(duration(std::time::Duration::from_millis(84)), "84 ms");
        assert_eq!(duration(std::time::Duration::from_millis(2400)), "2.4 s");
        assert_eq!(bytes(4180), "4.1 KB");
    }
}
