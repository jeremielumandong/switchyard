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
    pub id: ThemeId,
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
    /// Terminal colors 0–15.
    pub ansi16: [u32; 16],
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
            id: ThemeId::SwitchyardDark,
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
            ansi16: [
                0x2f2f35, 0xf05653, 0x5ec077, 0xebae42, 0x5aa3ec, 0xbd9ff2, 0x6ac5e8, 0xa1a1aa,
                0x6e6e77, 0xff7a73, 0x7fd894, 0xf7c76a, 0x82bbf5, 0xd3bdf8, 0x8fd8f2, 0xe7e7ea,
            ],
        }
    }

    /// The light theme.
    pub fn light() -> Self {
        Self {
            id: ThemeId::SwitchyardLight,
            dark: false,
            bg: c(0xf2f2f3),
            panel: c(0xf8f8f9),
            surface: c(0xffffff),
            elev: c(0xffffff),
            bd: c(0xe2e2e6),
            bd2: c(0xd2d2d8),
            hover: ca(0x0000000a),
            sel: alpha(0x2d74ca, 0.18),
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
            sx_cm: c(0x8a8a93),
            term: c(0xfbfbfc),
            scrim: ca(0x14141938),
            shadow: ca(0x00000024),
            ansi16: [
                0x18181b, 0xd02b31, 0x218a45, 0xa86500, 0x2971c6, 0x6e3ead, 0x00649e, 0x8a8a93,
                0x53535c, 0xe0474c, 0x2e9f55, 0xc57800, 0x3d86da, 0x8556c4, 0x0a7cbf, 0xb0b0b8,
            ],
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

    /// One of the 256 terminal colors. 0–15 follow the design's accents; 16–231 are the
    /// xterm color cube and 232–255 the gray ramp.
    pub fn ansi(&self, i: u8) -> Hsla {
        if i < 16 {
            return c(self.ansi16[i as usize]);
        }
        if i >= 232 {
            let v = 8 + 10 * (i as u32 - 232);
            return c(v << 16 | v << 8 | v);
        }
        let n = i as u32 - 16;
        let step = |x: u32| if x == 0 { 0 } else { 55 + 40 * x };
        c(step(n / 36) << 16 | step((n / 6) % 6) << 8 | step(n % 6))
    }

    /// Text color on a solid environment badge.
    pub fn env_on(&self, env: EnvironmentLabel) -> Hsla {
        match env {
            EnvironmentLabel::Staging => c(0x1a1406),
            _ => c(0xffffff),
        }
    }
}

/// A built-in theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ThemeId {
    /// The design's dark theme (default).
    SwitchyardDark,
    /// The design's light theme.
    SwitchyardLight,
    /// Nord.
    Nord,
    /// Dracula.
    Dracula,
    /// Catppuccin Mocha.
    CatppuccinMocha,
    /// Tokyo Night.
    TokyoNight,
    /// Gruvbox Dark.
    GruvboxDark,
    /// Maximum contrast on black.
    HighContrast,
    /// Catppuccin Latte.
    CatppuccinLatte,
    /// Solarized Light.
    SolarizedLight,
}

impl ThemeId {
    /// Every theme, in the order Settings → Appearance shows them.
    pub const ALL: [ThemeId; 10] = [
        ThemeId::SwitchyardDark,
        ThemeId::SwitchyardLight,
        ThemeId::Nord,
        ThemeId::Dracula,
        ThemeId::CatppuccinMocha,
        ThemeId::TokyoNight,
        ThemeId::GruvboxDark,
        ThemeId::HighContrast,
        ThemeId::CatppuccinLatte,
        ThemeId::SolarizedLight,
    ];

    /// Stable key for the `theme` setting.
    pub fn key(self) -> &'static str {
        match self {
            ThemeId::SwitchyardDark => "dark",
            ThemeId::SwitchyardLight => "light",
            ThemeId::Nord => "nord",
            ThemeId::Dracula => "dracula",
            ThemeId::CatppuccinMocha => "catppuccin-mocha",
            ThemeId::TokyoNight => "tokyo-night",
            ThemeId::GruvboxDark => "gruvbox-dark",
            ThemeId::HighContrast => "high-contrast",
            ThemeId::CatppuccinLatte => "catppuccin-latte",
            ThemeId::SolarizedLight => "solarized-light",
        }
    }

    /// Display name.
    pub fn label(self) -> &'static str {
        match self {
            ThemeId::SwitchyardDark => "Switchyard Dark",
            ThemeId::SwitchyardLight => "Switchyard Light",
            ThemeId::Nord => "Nord",
            ThemeId::Dracula => "Dracula",
            ThemeId::CatppuccinMocha => "Catppuccin Mocha",
            ThemeId::TokyoNight => "Tokyo Night",
            ThemeId::GruvboxDark => "Gruvbox Dark",
            ThemeId::HighContrast => "High Contrast",
            ThemeId::CatppuccinLatte => "Catppuccin Latte",
            ThemeId::SolarizedLight => "Solarized Light",
        }
    }

    /// The theme for a setting value (unknown values fall back to the default).
    pub fn from_key(key: &str) -> ThemeId {
        ThemeId::ALL
            .into_iter()
            .find(|t| t.key() == key)
            .unwrap_or(ThemeId::SwitchyardDark)
    }

    /// Its colors.
    pub fn palette(self) -> Palette {
        match self {
            ThemeId::SwitchyardDark => Palette::dark(),
            ThemeId::SwitchyardLight => Palette::light(),
            ThemeId::Nord => build(self, &NORD),
            ThemeId::Dracula => build(self, &DRACULA),
            ThemeId::CatppuccinMocha => build(self, &MOCHA),
            ThemeId::TokyoNight => build(self, &TOKYO),
            ThemeId::GruvboxDark => build(self, &GRUVBOX),
            ThemeId::HighContrast => build(self, &HIGH_CONTRAST),
            ThemeId::CatppuccinLatte => build(self, &LATTE),
            ThemeId::SolarizedLight => build(self, &SOLARIZED_LIGHT),
        }
    }
}

/// The few colors a theme defines; everything else is derived. Values were tuned so that
/// text reads at 7:1 or better on every background and on the selection (see the tests).
struct Spec {
    dark: bool,
    bg: u32,
    panel: u32,
    surface: u32,
    elev: u32,
    bd: u32,
    bd2: u32,
    fg: u32,
    fg2: u32,
    fg3: u32,
    acc: u32,
    acc_fg: u32,
    red: u32,
    yellow: u32,
    green: u32,
    blue: u32,
    magenta: u32,
    cyan: u32,
    kw: u32,
    string: u32,
    number: u32,
    func: u32,
    comment: u32,
    /// Opacity of the accent used as the selection background.
    sel: f32,
}

const NORD: Spec = Spec {
    dark: true,
    bg: 0x242933,
    panel: 0x2a303c,
    surface: 0x2e3440,
    elev: 0x3b4252,
    bd: 0x3b4252,
    bd2: 0x4c566a,
    fg: 0xeceff4,
    fg2: 0xd8dee9,
    fg3: 0x9aa5b9,
    acc: 0x88c0d0,
    acc_fg: 0x2e3440,
    red: 0xd57780,
    yellow: 0xebcb8b,
    green: 0xa3be8c,
    blue: 0x81a1c1,
    magenta: 0xb48ead,
    cyan: 0x88c0d0,
    kw: 0x8fb2cf,
    string: 0xa3be8c,
    number: 0xc4a0bd,
    func: 0x88c0d0,
    comment: 0x8a96ad,
    sel: 0.21,
};

const DRACULA: Spec = Spec {
    dark: true,
    bg: 0x1e1f29,
    panel: 0x21222c,
    surface: 0x282a36,
    elev: 0x343746,
    bd: 0x343746,
    bd2: 0x44475a,
    fg: 0xf8f8f2,
    fg2: 0xd0d2dc,
    fg3: 0x9aa1c4,
    acc: 0xbd93f9,
    acc_fg: 0x1e1f29,
    red: 0xff5555,
    yellow: 0xf1fa8c,
    green: 0x50fa7b,
    blue: 0x8be9fd,
    magenta: 0xff79c6,
    cyan: 0x8be9fd,
    kw: 0xff79c6,
    string: 0xf1fa8c,
    number: 0xbd93f9,
    func: 0x50fa7b,
    comment: 0x8a93c2,
    sel: 0.3,
};

const MOCHA: Spec = Spec {
    dark: true,
    bg: 0x11111b,
    panel: 0x181825,
    surface: 0x1e1e2e,
    elev: 0x313244,
    bd: 0x313244,
    bd2: 0x45475a,
    fg: 0xcdd6f4,
    fg2: 0xbac2de,
    fg3: 0x9399b2,
    acc: 0x89b4fa,
    acc_fg: 0x11111b,
    red: 0xf38ba8,
    yellow: 0xf9e2af,
    green: 0xa6e3a1,
    blue: 0x89b4fa,
    magenta: 0xcba6f7,
    cyan: 0x94e2d5,
    kw: 0xcba6f7,
    string: 0xa6e3a1,
    number: 0xfab387,
    func: 0x89b4fa,
    comment: 0x9399b2,
    sel: 0.23,
};

const TOKYO: Spec = Spec {
    dark: true,
    bg: 0x13131a,
    panel: 0x16161e,
    surface: 0x1a1b26,
    elev: 0x24283b,
    bd: 0x24283b,
    bd2: 0x3b4261,
    fg: 0xc0caf5,
    fg2: 0xa9b1d6,
    fg3: 0x8189b5,
    acc: 0x7aa2f7,
    acc_fg: 0x16161e,
    red: 0xf7768e,
    yellow: 0xe0af68,
    green: 0x9ece6a,
    blue: 0x7aa2f7,
    magenta: 0xbb9af7,
    cyan: 0x7dcfff,
    kw: 0xbb9af7,
    string: 0x9ece6a,
    number: 0xff9e64,
    func: 0x7aa2f7,
    comment: 0x7f88b6,
    sel: 0.22,
};

const GRUVBOX: Spec = Spec {
    dark: true,
    bg: 0x1d2021,
    panel: 0x202324,
    surface: 0x282828,
    elev: 0x32302f,
    bd: 0x3c3836,
    bd2: 0x504945,
    fg: 0xebdbb2,
    fg2: 0xd5c4a1,
    fg3: 0xa89984,
    acc: 0x83a598,
    acc_fg: 0x1d2021,
    red: 0xfb5f4a,
    yellow: 0xfabd2f,
    green: 0xb8bb26,
    blue: 0x83a598,
    magenta: 0xd3869b,
    cyan: 0x8ec07c,
    kw: 0xfb6a56,
    string: 0xb8bb26,
    number: 0xd3869b,
    func: 0x8ec07c,
    comment: 0x928374,
    sel: 0.24,
};

const HIGH_CONTRAST: Spec = Spec {
    dark: true,
    bg: 0x000000,
    panel: 0x0a0a0a,
    surface: 0x050505,
    elev: 0x141414,
    bd: 0x3a3a3a,
    bd2: 0x5a5a5a,
    fg: 0xffffff,
    fg2: 0xe0e0e0,
    fg3: 0xb8b8b8,
    acc: 0x4fc1ff,
    acc_fg: 0x000000,
    red: 0xff6b6b,
    yellow: 0xffd75f,
    green: 0x7ee787,
    blue: 0x4fc1ff,
    magenta: 0xd7a6ff,
    cyan: 0x7fdbff,
    kw: 0xd7a6ff,
    string: 0x9ef09e,
    number: 0xffcb6b,
    func: 0x7fdbff,
    comment: 0xb8b8b8,
    sel: 0.4,
};

const LATTE: Spec = Spec {
    dark: false,
    bg: 0xe6e9ef,
    panel: 0xeceef3,
    surface: 0xeff1f5,
    elev: 0xf7f8fa,
    bd: 0xccd0da,
    bd2: 0xbcc0cc,
    fg: 0x3a3d55,
    fg2: 0x53566e,
    fg3: 0x6c6f85,
    acc: 0x1e66f5,
    acc_fg: 0xffffff,
    red: 0xd20f39,
    yellow: 0xa8650a,
    green: 0x2f7d1e,
    blue: 0x1e66f5,
    magenta: 0x8839ef,
    cyan: 0x137a80,
    kw: 0x7a2fd8,
    string: 0x2f7d1e,
    number: 0xa84607,
    func: 0x1e5fe0,
    comment: 0x7c7f93,
    sel: 0.2,
};

const SOLARIZED_LIGHT: Spec = Spec {
    dark: false,
    bg: 0xeee8d5,
    panel: 0xf5efdc,
    surface: 0xfdf6e3,
    elev: 0xfffbef,
    bd: 0xe4dcc4,
    bd2: 0xd6cdb2,
    fg: 0x073642,
    fg2: 0x47595f,
    fg3: 0x5f7175,
    acc: 0x1b6fae,
    acc_fg: 0xffffff,
    red: 0xc52a27,
    yellow: 0x8f6c00,
    green: 0x667500,
    blue: 0x1b6fae,
    magenta: 0xb32c6f,
    cyan: 0x17736d,
    kw: 0x5c6a00,
    string: 0x17736d,
    number: 0xb32c6f,
    func: 0x1b6fae,
    comment: 0x6f8083,
    sel: 0.22,
};

fn build(id: ThemeId, s: &Spec) -> Palette {
    let fg_a = |a: f32| alpha(s.fg, a);
    Palette {
        id,
        dark: s.dark,
        bg: c(s.bg),
        panel: c(s.panel),
        surface: c(s.surface),
        elev: c(s.elev),
        bd: c(s.bd),
        bd2: c(s.bd2),
        hover: fg_a(0.05),
        sel: alpha(s.acc, s.sel),
        line: fg_a(0.04),
        fg: c(s.fg),
        fg2: c(s.fg2),
        fg3: c(s.fg3),
        acc: c(s.acc),
        acc_fg: c(s.acc_fg),
        prod: c(s.red),
        stg: c(s.yellow),
        dev: c(s.green),
        loc: c(s.fg3),
        prod_bg: alpha(s.red, 0.14),
        stg_bg: alpha(s.yellow, 0.15),
        dev_bg: alpha(s.green, 0.13),
        staged: alpha(s.yellow, if s.dark { 0.18 } else { 0.28 }),
        sx_kw: c(s.kw),
        sx_str: c(s.string),
        sx_num: c(s.number),
        sx_fn: c(s.func),
        sx_cm: c(s.comment),
        term: c(if s.dark { s.bg } else { s.surface }),
        scrim: if s.dark {
            ca(0x00000080)
        } else {
            ca(0x14141938)
        },
        shadow: if s.dark {
            ca(0x0000008c)
        } else {
            ca(0x00000024)
        },
        ansi16: if s.dark {
            [
                s.bd2, s.red, s.green, s.yellow, s.blue, s.magenta, s.cyan, s.fg2, s.fg3, s.red,
                s.green, s.yellow, s.blue, s.magenta, s.cyan, s.fg,
            ]
        } else {
            [
                s.fg, s.red, s.green, s.yellow, s.blue, s.magenta, s.cyan, s.fg3, s.fg2, s.red,
                s.green, s.yellow, s.blue, s.magenta, s.cyan, s.bd2,
            ]
        },
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
        "name": p.id.label(),
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
pub fn apply(p: Palette, mut window: Option<&mut Window>, cx: &mut App) {
    cx.set_global(ActivePalette(p));
    Theme::change(
        if p.dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        },
        window.as_deref_mut(),
        cx,
    );
    let hl = highlight_theme(&p);
    Theme::update(cx, |t| {
        t.font_family = SharedString::from(SANS);
        t.mono_font_family = SharedString::from(MONO);
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
    apply_fonts(window, cx);
}

/// Restyle gpui-component's font sizes and the editor font for the active
/// [`crate::appearance`] settings (zoom, editor font), then redraw.
pub fn apply_fonts(window: Option<&mut Window>, cx: &mut App) {
    let (ui, mono_family, mono) = (
        crate::appearance::ui_font_size(cx),
        crate::appearance::editor_font_family(cx),
        crate::appearance::editor_font_size(cx),
    );
    Theme::update(cx, |t| {
        t.font_size = ui;
        t.mono_font_family = mono_family;
        t.mono_font_size = mono;
    });
    match window {
        Some(w) => w.refresh(),
        None => cx.refresh_windows(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lin(v: f32) -> f32 {
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }

    fn lum(h: Hsla) -> f32 {
        let r = Rgba::from(h);
        0.2126 * lin(r.r) + 0.7152 * lin(r.g) + 0.0722 * lin(r.b)
    }

    /// `top` (with its alpha) over the opaque `bottom`.
    fn over(top: Hsla, bottom: Hsla) -> Hsla {
        let (t, b) = (Rgba::from(top), Rgba::from(bottom));
        let mix = |x: f32, y: f32| x * t.a + y * (1.0 - t.a);
        Rgba {
            r: mix(t.r, b.r),
            g: mix(t.g, b.g),
            b: mix(t.b, b.b),
            a: 1.0,
        }
        .into()
    }

    fn ratio(a: Hsla, b: Hsla) -> f32 {
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn every_theme_is_readable() {
        let mut bad = Vec::new();
        let mut check = |ok: bool, msg: String| {
            if !ok {
                bad.push(msg);
            }
        };
        for id in ThemeId::ALL {
            let p = id.palette();
            let name = id.label();
            assert_eq!(ThemeId::from_key(id.key()), id);
            for (bg, what) in [(p.bg, "bg"), (p.panel, "panel"), (p.surface, "surface")] {
                check(ratio(p.fg, bg) >= 7.0, format!("{name}: text on {what}"));
                check(
                    ratio(p.fg2, bg) >= 4.5,
                    format!("{name}: secondary text on {what}"),
                );
            }
            check(
                ratio(p.fg3, p.surface) >= 3.0,
                format!("{name}: muted text"),
            );
            // Selected text stays readable, and the selection is visible.
            let sel = over(p.sel, p.surface);
            let r = ratio(p.fg, sel);
            check(r >= 7.0, format!("{name}: text on selection {r:.1}"));
            let v = ratio(sel, p.surface);
            check(v >= 1.15, format!("{name}: selection visibility {v:.2}"));
            check(
                ratio(p.acc_fg, p.acc) >= 4.5,
                format!("{name}: button text"),
            );
            for (c, what) in [
                (p.sx_kw, "keyword"),
                (p.sx_str, "string"),
                (p.sx_num, "number"),
                (p.sx_fn, "function"),
            ] {
                let r = ratio(c, p.surface);
                check(r >= 4.5, format!("{name}: {what} {r:.1}"));
            }
            check(
                ratio(p.sx_cm, p.surface) >= 2.8,
                format!("{name}: comments"),
            );
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }
}
