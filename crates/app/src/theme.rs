//! Design tokens from the Switchyard design (`docs/design/Switchyard.dc.html`), dark and
//! light, plus the bridge into gpui-component's theme so built-in controls match.

use std::sync::Arc;

use gpui_kit::component::highlighter::HighlightTheme;
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Global, Hsla, Rgba, SharedString, Window, px, rgb, rgba};
use switchyard_core::store::EnvironmentLabel;

/// Sans-serif UI font.
pub const SANS: &str = "Geist";
/// Monospace font (editor, grid, badges).
pub const MONO: &str = "Geist Mono";

/// Colors used across the app. Names follow the CSS variables in the design.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub dark: bool,
    pub bg: Hsla,
    pub panel: Hsla,
    pub surface: Hsla,
    pub elev: Hsla,
    pub bd: Hsla,
    pub bd2: Hsla,
    pub hover: Hsla,
    pub sel: Hsla,
    pub line: Hsla,
    pub fg: Hsla,
    pub fg2: Hsla,
    pub fg3: Hsla,
    pub acc: Hsla,
    pub acc_fg: Hsla,
    pub prod: Hsla,
    pub stg: Hsla,
    pub dev: Hsla,
    pub loc: Hsla,
    pub prod_bg: Hsla,
    pub stg_bg: Hsla,
    pub dev_bg: Hsla,
    pub staged: Hsla,
    pub sx_kw: Hsla,
    pub sx_str: Hsla,
    pub sx_num: Hsla,
    pub sx_fn: Hsla,
    pub sx_cm: Hsla,
    pub term: Hsla,
    pub scrim: Hsla,
    pub shadow: Hsla,
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

fn ca(hex_rgba: u32) -> Hsla {
    rgba(hex_rgba).into()
}

fn alpha(hex: u32, a: f32) -> Hsla {
    let mut h: Hsla = rgb(hex).into();
    h.a = a;
    h
}

impl Palette {
    /// The dark theme (primary).
    pub fn dark() -> Self {
        Self {
            dark: true,
            bg: c(0x0e0e10),
            panel: c(0x131316),
            surface: c(0x17171a),
            elev: c(0x1d1d21),
            bd: c(0x232328),
            bd2: c(0x2f2f35),
            hover: ca(0xffffff0b),
            sel: alpha(0x549de5, 0.2),
            line: ca(0xffffff09),
            fg: c(0xe7e7ea),
            fg2: c(0xa1a1aa),
            fg3: c(0x6e6e77),
            acc: c(0x5aa3ec),
            acc_fg: c(0x0b0b0d),
            prod: c(0xf05653),
            stg: c(0xebae42),
            dev: c(0x5ec077),
            loc: c(0x8b8b94),
            prod_bg: alpha(0xf05653, 0.13),
            stg_bg: alpha(0xebae42, 0.13),
            dev_bg: alpha(0x5ec077, 0.12),
            staged: alpha(0xebae42, 0.17),
            sx_kw: c(0xbd9ff2),
            sx_str: c(0x85cc87),
            sx_num: c(0xefaf6f),
            sx_fn: c(0x6ac5e8),
            sx_cm: c(0x6e6e77),
            term: c(0x0b0b0d),
            scrim: ca(0x00000080),
            shadow: ca(0x0000008c),
        }
    }

    /// The light theme.
    pub fn light() -> Self {
        Self {
            dark: false,
            bg: c(0xf2f2f3),
            panel: c(0xf8f8f9),
            surface: c(0xffffff),
            elev: c(0xffffff),
            bd: c(0xe2e2e6),
            bd2: c(0xd2d2d8),
            hover: ca(0x0000000a),
            sel: alpha(0x2d74ca, 0.12),
            line: ca(0x00000007),
            fg: c(0x18181b),
            fg2: c(0x53535c),
            fg3: c(0x8a8a93),
            acc: c(0x2971c6),
            acc_fg: c(0xffffff),
            prod: c(0xd02b31),
            stg: c(0xc57800),
            dev: c(0x218a45),
            loc: c(0x8a8a93),
            prod_bg: alpha(0xd02b31, 0.09),
            stg_bg: alpha(0xd98b09, 0.13),
            dev_bg: alpha(0x218a45, 0.09),
            staged: alpha(0xf0b135, 0.28),
            sx_kw: c(0x6e3ead),
            sx_str: c(0x27762f),
            sx_num: c(0xad5600),
            sx_fn: c(0x00649e),
            sx_cm: c(0x9a9aa2),
            term: c(0xfbfbfc),
            scrim: ca(0x14141938),
            shadow: ca(0x00000024),
        }
    }

    /// The accent color of an environment label.
    pub fn env(&self, env: EnvironmentLabel) -> Hsla {
        match env {
            EnvironmentLabel::Production => self.prod,
            EnvironmentLabel::Staging => self.stg,
            EnvironmentLabel::Development => self.dev,
            EnvironmentLabel::Local => self.loc,
        }
    }

    /// The tinted background of an environment label.
    pub fn env_bg(&self, env: EnvironmentLabel) -> Hsla {
        match env {
            EnvironmentLabel::Production => self.prod_bg,
            EnvironmentLabel::Staging => self.stg_bg,
            EnvironmentLabel::Development => self.dev_bg,
            EnvironmentLabel::Local => self.hover,
        }
    }

    /// Text color on a solid environment badge.
    pub fn env_on(&self, env: EnvironmentLabel) -> Hsla {
        match env {
            EnvironmentLabel::Staging => c(0x1a1406),
            _ => c(0xffffff),
        }
    }
}

/// The active palette, as a GPUI global.
#[derive(Clone, Copy, Debug)]
pub struct ActivePalette(pub Palette);

impl Global for ActivePalette {}

/// Read the active palette.
pub fn palette(cx: &App) -> Palette {
    cx.try_global::<ActivePalette>()
        .map(|p| p.0)
        .unwrap_or_else(Palette::dark)
}

fn hex(h: Hsla) -> String {
    let r = Rgba::from(h);
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        (r.r * 255.0).round() as u8,
        (r.g * 255.0).round() as u8,
        (r.b * 255.0).round() as u8,
        (r.a * 255.0).round() as u8
    )
}

fn highlight_theme(p: &Palette) -> Option<HighlightTheme> {
    let style = |h: Hsla| serde_json::json!({ "color": hex(h) });
    let json = serde_json::json!({
        "name": if p.dark { "Switchyard Dark" } else { "Switchyard Light" },
        "appearance": if p.dark { "dark" } else { "light" },
        "style": {
            "editor.background": hex(p.surface),
            "editor.foreground": hex(p.fg),
            "editor.active_line.background": hex(p.line),
            "editor.line_number": hex(p.fg3),
            "editor.active_line_number": hex(p.fg),
            "editor.invisible": hex(p.fg3),
            "editor.gutter.background": hex(p.surface),
            "error": hex(p.prod),
            "warning": hex(p.stg),
            "info": hex(p.acc),
            "hint": hex(p.fg3),
            "success": hex(p.dev),
            "syntax": {
                "keyword": style(p.sx_kw),
                "string": style(p.sx_str),
                "number": style(p.sx_num),
                "boolean": style(p.sx_num),
                "constant": style(p.sx_num),
                "function": style(p.sx_fn),
                "comment": { "color": hex(p.sx_cm), "font_style": "italic" },
                "operator": style(p.fg2),
                "punctuation": style(p.fg2),
                "punctuation.bracket": style(p.fg2),
                "punctuation.delimiter": style(p.fg2),
                "type": style(p.sx_fn),
                "variable": style(p.fg),
                "property": style(p.fg),
                "attribute": style(p.fg),
            }
        }
    });
    serde_json::from_value(json).ok()
}

/// Apply a palette: store it globally and restyle gpui-component to match.
pub fn apply(p: Palette, window: Option<&mut Window>, cx: &mut App) {
    cx.set_global(ActivePalette(p));
    Theme::change(
        if p.dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        },
        window,
        cx,
    );
    let hl = highlight_theme(&p);
    Theme::update(cx, |t| {
        t.font_family = SharedString::from(SANS);
        t.mono_font_family = SharedString::from(MONO);
        t.font_size = px(13.);
        t.mono_font_size = px(12.5);
        t.radius = px(6.);
        t.radius_lg = px(10.);
        t.shadow = true;
        t.focus_ring = false;
        t.background = p.surface;
        t.foreground = p.fg;
        t.border = p.bd2;
        t.input = p.bd2;
        t.ring = p.acc;
        t.caret = p.acc;
        t.selection = p.sel;
        t.primary = p.acc;
        t.primary_hover = p.acc;
        t.primary_active = p.acc;
        t.primary_foreground = p.acc_fg;
        t.secondary = p.elev;
        t.secondary_hover = p.hover;
        t.secondary_active = p.hover;
        t.secondary_foreground = p.fg;
        t.muted = p.hover;
        t.muted_foreground = p.fg3;
        t.popover = p.elev;
        t.popover_foreground = p.fg;
        t.accent = p.sel;
        t.accent_foreground = p.fg;
        t.list_hover = p.hover;
        t.list_active = p.sel;
        t.list_active_border = p.acc;
        t.danger = p.prod;
        t.warning = p.stg;
        t.success = p.dev;
        t.info = p.acc;
        t.scrollbar = Hsla::transparent_black();
        t.scrollbar_thumb = p.bd2;
        t.scrollbar_thumb_hover = p.fg3;
        t.title_bar = p.panel;
        t.title_bar_border = p.bd;
        t.sidebar = p.panel;
        t.sidebar_border = p.bd;
        t.tab_bar = p.panel;
        t.table = p.surface;
        t.table_head = p.panel;
        t.table_row_border = p.line;
        if let Some(hl) = hl {
            t.highlight_theme = Arc::new(hl);
        }
    });
}
