//! "Follow Omarchy": read the active Omarchy theme (`~/.config/omarchy/current/theme`)
//! and turn its terminal colors into a [`Palette`].
//!
//! Omarchy switches themes by replacing that directory (a symlink to
//! `~/.config/omarchy/themes/<name>`), so the workspace re-reads it every few seconds on
//! the core runtime and re-applies the palette when the colors change. Newer themes carry
//! a `colors.toml` (`background`, `foreground`, `accent`, `color0`..`color15`); every theme
//! has an `alacritty.toml`, read when `colors.toml` is missing. A `light.mode` file marks a
//! light theme.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::{App, Global, rgb};

use crate::theme::{Palette, Spec, ThemeId, build};

/// How often the theme directory is re-read.
pub const POLL: Duration = Duration::from_secs(2);

/// The colors of an Omarchy theme.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OmarchyColors {
    /// Display name ("Tokyo Night").
    pub name: String,
    /// The theme says it is light (`light.mode`, or a light background).
    pub light: bool,
    pub background: u32,
    pub foreground: u32,
    /// The theme's accent, when it names one.
    pub accent: Option<u32>,
    /// Terminal colors 0–15.
    pub ansi: [u32; 16],
}

/// The last Omarchy theme read, as a GPUI global.
#[derive(Clone, Debug, Default)]
pub struct OmarchyTheme {
    pub colors: Option<OmarchyColors>,
    pub palette: Option<Palette>,
}

impl Global for OmarchyTheme {}

/// The palette built from the active Omarchy theme, once it has been read.
pub fn palette(cx: &App) -> Option<Palette> {
    cx.try_global::<OmarchyTheme>().and_then(|t| t.palette)
}

/// The active Omarchy theme's name, once it has been read.
pub fn name(cx: &App) -> Option<String> {
    cx.try_global::<OmarchyTheme>()
        .and_then(|t| t.colors.as_ref())
        .map(|c| c.name.clone())
}

/// Store a freshly read theme. Returns its palette.
pub fn set(colors: Option<OmarchyColors>, cx: &mut App) -> Option<Palette> {
    let palette = colors.as_ref().map(build_palette);
    cx.set_global(OmarchyTheme { colors, palette });
    palette
}

/// Whether to watch for an Omarchy theme at all (Omarchy is an Arch Linux setup).
pub fn supported() -> bool {
    cfg!(target_os = "linux")
}

/// `~/.config/omarchy/current/theme`.
fn theme_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/omarchy/current/theme"))
}

/// Read the active Omarchy theme. Disk I/O: run it on the core runtime.
pub fn load() -> Option<OmarchyColors> {
    read_theme(&theme_dir()?)
}

fn read_theme(dir: &Path) -> Option<OmarchyColors> {
    let colors = std::fs::read_to_string(dir.join("colors.toml"))
        .ok()
        .and_then(|s| from_colors_toml(&s))
        .or_else(|| {
            std::fs::read_to_string(dir.join("alacritty.toml"))
                .ok()
                .and_then(|s| from_alacritty(&s))
        })?;
    // Newer Omarchy writes the name next to the theme; older ones symlink the directory.
    let slug = dir
        .parent()
        .and_then(|p| std::fs::read_to_string(p.join("theme.name")).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::fs::canonicalize(dir)
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        })
        .unwrap_or_default();
    let light = dir.join("light.mode").exists() || lum(colors.background) > 0.4;
    Some(OmarchyColors {
        name: display_name(&slug),
        light,
        ..colors
    })
}

/// `tokyo-night` → `Tokyo Night`.
fn display_name(slug: &str) -> String {
    slug.split(['-', '_', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut ch = w.chars();
            match ch.next() {
                Some(f) => f.to_uppercase().chain(ch).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `key = value` pairs of a TOML file, keys prefixed with their `[table]` (`colors.primary.background`).
/// Enough for theme files: no arrays, inline tables or multi-line strings.
fn toml_pairs(src: &str) -> Vec<(String, String)> {
    let mut table = String::new();
    let mut out = Vec::new();
    for line in src.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            table = rest
                .trim_start_matches('[')
                .split(']')
                .next()
                .unwrap_or("")
                .trim()
                .replace(['"', '\''], "");
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().replace(['"', '\''], "");
        let value = value.trim();
        let value = match value.chars().next() {
            Some(q @ ('"' | '\'')) => value[1..].split(q).next().unwrap_or(""),
            _ => value.split(['#', ' ', '\t']).next().unwrap_or(""),
        };
        let full = if table.is_empty() {
            key
        } else {
            format!("{table}.{key}")
        };
        out.push((full, value.to_owned()));
    }
    out
}

/// `#rrggbb`, `0xrrggbb` or `rrggbb` (an alpha byte after it is ignored).
fn parse_hex(v: &str) -> Option<u32> {
    let v = v.trim();
    let v = v
        .strip_prefix('#')
        .or_else(|| v.strip_prefix("0x"))
        .or_else(|| v.strip_prefix("0X"))
        .unwrap_or(v);
    if !(v.len() == 6 || v.len() == 8) || !v.is_ascii() {
        return None;
    }
    u32::from_str_radix(&v[..6], 16).ok()
}

fn lookup(pairs: &[(String, String)], key: &str) -> Option<u32> {
    pairs
        .iter()
        .rev()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| parse_hex(v))
}

/// Fill missing terminal colors: bright from normal, normal from Switchyard Dark.
fn complete_ansi(found: [Option<u32>; 16]) -> [u32; 16] {
    let fallback = Palette::dark().ansi16;
    let mut out = [0; 16];
    for i in 0..16 {
        out[i] = found[i]
            .or(if i >= 8 { found[i - 8] } else { None })
            .unwrap_or(fallback[i]);
    }
    out
}

/// Omarchy's `colors.toml`.
fn from_colors_toml(src: &str) -> Option<OmarchyColors> {
    let pairs = toml_pairs(src);
    let mut ansi = [None; 16];
    for (i, slot) in ansi.iter_mut().enumerate() {
        *slot = lookup(&pairs, &format!("color{i}"));
    }
    Some(OmarchyColors {
        name: String::new(),
        light: false,
        background: lookup(&pairs, "background")?,
        foreground: lookup(&pairs, "foreground")?,
        accent: lookup(&pairs, "accent"),
        ansi: complete_ansi(ansi),
    })
}

/// A theme's `alacritty.toml` (`[colors.primary]`, `[colors.normal]`, `[colors.bright]`).
fn from_alacritty(src: &str) -> Option<OmarchyColors> {
    const NAMES: [&str; 8] = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    ];
    let pairs = toml_pairs(src);
    let mut ansi = [None; 16];
    for (i, n) in NAMES.iter().enumerate() {
        ansi[i] = lookup(&pairs, &format!("colors.normal.{n}"));
        ansi[i + 8] = lookup(&pairs, &format!("colors.bright.{n}"));
    }
    Some(OmarchyColors {
        name: String::new(),
        light: false,
        background: lookup(&pairs, "colors.primary.background")?,
        foreground: lookup(&pairs, "colors.primary.foreground")?,
        accent: None,
        ansi: complete_ansi(ansi),
    })
}

fn channels(c: u32) -> [f32; 3] {
    [(c >> 16) & 0xff, (c >> 8) & 0xff, c & 0xff].map(|v| v as f32)
}

/// `a` moved towards `b` by `t` (0–1).
fn mix(a: u32, b: u32, t: f32) -> u32 {
    let (x, y) = (channels(a), channels(b));
    let ch = |i: usize| (x[i] + (y[i] - x[i]) * t).round().clamp(0.0, 255.0) as u32;
    ch(0) << 16 | ch(1) << 8 | ch(2)
}

fn lum(c: u32) -> f32 {
    let lin = |v: f32| {
        let v = v / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    let [r, g, b] = channels(c);
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

fn ratio(a: u32, b: u32) -> f32 {
    let (x, y) = (lum(a), lum(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// `color`, moved towards `toward` until it reads at `min`:1 on `bg` (or as far as it goes).
fn readable(color: u32, bg: u32, toward: u32, min: f32) -> u32 {
    (0..=10)
        .map(|i| mix(color, toward, i as f32 / 10.0))
        .find(|c| ratio(*c, bg) >= min)
        .unwrap_or(toward)
}

/// The Switchyard palette for an Omarchy theme: its background is the editor surface,
/// panels are shaded from it, and its terminal colors drive status and syntax colors.
pub fn build_palette(c: &OmarchyColors) -> Palette {
    let dark = !c.light;
    let (s, fg, a) = (c.background, c.foreground, c.ansi);
    let (bg, panel, elev) = if dark {
        (mix(s, 0, 0.25), mix(s, 0, 0.14), mix(s, fg, 0.08))
    } else {
        (mix(s, fg, 0.07), mix(s, fg, 0.035), mix(s, 0xffffff, 0.5))
    };
    let acc = c.accent.unwrap_or(a[4]);
    let acc_fg = [s, fg, 0x000000, 0xffffff]
        .into_iter()
        .find(|t| ratio(*t, acc) >= 4.5)
        .unwrap_or(if lum(acc) > 0.18 { 0x000000 } else { 0xffffff });
    let text = |color: u32, min: f32| readable(color, s, fg, min);
    let fg3 = text(mix(fg, s, 0.42), 3.0);
    let spec = Spec {
        dark,
        bg,
        panel,
        surface: s,
        elev,
        bd: mix(s, fg, 0.12),
        bd2: mix(s, fg, 0.22),
        fg,
        fg2: text(mix(fg, s, 0.2), 4.5),
        fg3,
        acc,
        acc_fg,
        red: a[1],
        yellow: a[3],
        green: a[2],
        blue: a[4],
        magenta: a[5],
        cyan: a[6],
        kw: text(a[5], 4.5),
        string: text(a[2], 4.5),
        number: text(a[3], 4.5),
        func: text(a[4], 4.5),
        comment: text(mix(fg, s, 0.5), 2.8),
        sel: if dark { 0.25 } else { 0.2 },
    };
    let mut p = build(ThemeId::Omarchy, &spec);
    // The terminal looks like Omarchy's own terminal.
    p.ansi16 = c.ansi;
    p.term = rgb(s).into();
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLORS_TOML: &str = r##"
accent = "#7aa2f7"
cursor = "#c0caf5"
foreground = "#a9b1d6"
background = "#1a1b26"
selection_foreground = "#c0caf5"
selection_background = "#7aa2f7"

color0 = "#32344a"
color1 = "#f7768e"
color2 = "#9ece6a"
color3 = "#e0af68"
color4 = "#7aa2f7"
color5 = "#ad8ee6"
color6 = "#449dab"
color7 = "#787c99"
color8 = "#444b6a"
color9 = "#ff7a93"
color10 = "#b9f27c"
color11 = "#ff9e64"
color12 = "#7da6ff"
color13 = "#bb9af7"
color14 = "#0db9d7"
color15 = "#acb0d0"
"##;

    const ALACRITTY: &str = r##"
[colors]
[colors.primary]
background = '#eff1f5' # base
foreground = "0x4c4f69"

[colors.normal]
black = "#5c5f77"
red = "#d20f39"
green = "#40a02b"
yellow = "#df8e1d"
blue = "#1e66f5"
magenta = "#ea76cb"
cyan = "#179299"
white = "#acb0be"

[colors.bright]
black = "#6c6f85"
red = "#d20f39"
"##;

    #[test]
    fn reads_colors_toml() {
        let c = from_colors_toml(COLORS_TOML).expect("parsed");
        assert_eq!(c.background, 0x1a1b26);
        assert_eq!(c.foreground, 0xa9b1d6);
        assert_eq!(c.accent, Some(0x7aa2f7));
        assert_eq!(c.ansi[1], 0xf7768e);
        assert_eq!(c.ansi[15], 0xacb0d0);
    }

    #[test]
    fn reads_alacritty_and_fills_missing_bright_colors() {
        let c = from_alacritty(ALACRITTY).expect("parsed");
        assert_eq!(c.background, 0xeff1f5);
        assert_eq!(c.foreground, 0x4c4f69);
        assert_eq!(c.accent, None);
        assert_eq!(c.ansi[4], 0x1e66f5);
        assert_eq!(c.ansi[8], 0x6c6f85);
        // No bright green: the normal one.
        assert_eq!(c.ansi[10], 0x40a02b);
    }

    #[test]
    fn missing_background_is_not_a_theme() {
        assert_eq!(from_colors_toml("foreground = \"#ffffff\""), None);
        assert_eq!(
            from_alacritty("[colors.primary]\nbackground = \"nope\""),
            None
        );
    }

    #[test]
    fn reads_a_theme_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        let themes = root.path().join("themes/catppuccin-latte");
        std::fs::create_dir_all(&themes).expect("mkdir");
        std::fs::write(themes.join("alacritty.toml"), ALACRITTY).expect("write");
        std::fs::write(themes.join("light.mode"), "").expect("write");
        let current = root.path().join("current");
        std::fs::create_dir_all(&current).expect("mkdir");
        #[cfg(unix)]
        let link = {
            let link = current.join("theme");
            std::os::unix::fs::symlink(&themes, &link).expect("symlink");
            link
        };
        #[cfg(not(unix))]
        let link = themes.clone();
        let c = read_theme(&link).expect("read");
        assert_eq!(c.name, "Catppuccin Latte");
        assert!(c.light);
        assert!(read_theme(&root.path().join("missing")).is_none());
    }

    #[test]
    fn palettes_stay_readable() {
        for c in [
            from_colors_toml(COLORS_TOML),
            from_alacritty(ALACRITTY).map(|c| OmarchyColors { light: true, ..c }),
        ] {
            let c = c.expect("parsed");
            let p = build_palette(&c);
            assert_eq!(p.id, ThemeId::Omarchy);
            assert_eq!(p.dark, !c.light);
            let s = c.background;
            let hex = |h: gpui_kit::Hsla| {
                let r = gpui_kit::Rgba::from(h);
                let b = |v: f32| (v * 255.0).round() as u32;
                b(r.r) << 16 | b(r.g) << 8 | b(r.b)
            };
            assert!(ratio(hex(p.fg2), s) >= 4.5);
            assert!(ratio(hex(p.fg3), s) >= 3.0);
            for color in [p.sx_kw, p.sx_str, p.sx_num, p.sx_fn] {
                assert!(ratio(hex(color), s) >= 4.5);
            }
            assert!(ratio(hex(p.acc_fg), hex(p.acc)) >= 4.5);
            assert_eq!(p.ansi16, c.ansi);
        }
    }

    #[test]
    fn names_from_slugs() {
        assert_eq!(display_name("tokyo-night"), "Tokyo Night");
        assert_eq!(display_name("rose_pine"), "Rose Pine");
        assert_eq!(display_name(""), "");
    }
}
