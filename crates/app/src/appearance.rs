//! Text size and zoom (Settings → Appearance): the editor font, its size and the UI zoom
//! level. Saved under [`APPEARANCE_KEY`] in the settings store and kept in a GPUI global
//! that the theme, editors, result grids and terminals read when they lay out.

use gpui_kit::component::Size;
use gpui_kit::{App, Global, Pixels, SharedString, px};
use serde::{Deserialize, Serialize};

use crate::theme::MONO;

/// Settings-store key.
pub const APPEARANCE_KEY: &str = "appearance";
/// Smallest zoom level.
pub const ZOOM_MIN: f32 = 0.7;
/// Largest zoom level.
pub const ZOOM_MAX: f32 = 2.0;
/// The levels zoom in / zoom out step through.
const ZOOM_STEPS: [f32; 10] = [0.7, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 1.9, 2.0];
/// Editor font size bounds (px, before zoom).
pub const EDITOR_FONT_MIN: f32 = 8.0;
/// See [`EDITOR_FONT_MIN`].
pub const EDITOR_FONT_MAX: f32 = 32.0;
/// UI font size at 100 %.
const UI_FONT: f32 = 13.0;
/// Result-grid row height at 100 % (gpui-component's `Size::XSmall`).
const GRID_ROW: f32 = 26.0;
/// Terminal line height per px of font (19 px lines for the 12.5 px default).
const TERM_LINE_RATIO: f32 = 19.0 / 12.5;

/// Editor font and zoom, as saved.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    /// Monospace family for SQL / file editors, the result grid and terminals.
    pub editor_font_family: String,
    /// Editor font size in px at 100 % zoom.
    pub editor_font_size: f32,
    /// UI zoom, 1.0 = 100 %.
    pub zoom: f32,
}

impl Default for AppearanceSettings {
    fn default() -> Self {
        Self {
            editor_font_family: MONO.to_owned(),
            editor_font_size: 12.5,
            zoom: 1.0,
        }
    }
}

impl AppearanceSettings {
    /// The same settings with every value in range (hand-edited or older stores).
    pub fn sanitized(mut self) -> Self {
        self.zoom = clamp_zoom(self.zoom);
        self.editor_font_size = clamp_font_size(self.editor_font_size);
        let family = self.editor_font_family.trim();
        self.editor_font_family = if family.is_empty() {
            MONO.to_owned()
        } else {
            family.to_owned()
        };
        self
    }
}

/// `z` within [`ZOOM_MIN`]..=[`ZOOM_MAX`]; anything not a number is 100 %.
pub fn clamp_zoom(z: f32) -> f32 {
    if z.is_finite() {
        z.clamp(ZOOM_MIN, ZOOM_MAX)
    } else {
        1.0
    }
}

/// `s` within the editor font bounds, rounded to half pixels.
pub fn clamp_font_size(s: f32) -> f32 {
    if s.is_finite() {
        ((s * 2.0).round() / 2.0).clamp(EDITOR_FONT_MIN, EDITOR_FONT_MAX)
    } else {
        AppearanceSettings::default().editor_font_size
    }
}

/// The next zoom level up from `z`.
pub fn zoom_in(z: f32) -> f32 {
    let z = clamp_zoom(z);
    ZOOM_STEPS
        .iter()
        .copied()
        .find(|s| *s > z + 0.001)
        .unwrap_or(ZOOM_MAX)
}

/// The next zoom level down from `z`.
pub fn zoom_out(z: f32) -> f32 {
    let z = clamp_zoom(z);
    ZOOM_STEPS
        .iter()
        .rev()
        .copied()
        .find(|s| *s < z - 0.001)
        .unwrap_or(ZOOM_MIN)
}

/// `z` as a whole percentage, for labels.
pub fn percent(z: f32) -> u32 {
    (clamp_zoom(z) * 100.0).round() as u32
}

/// The active settings, as a GPUI global.
#[derive(Default)]
pub struct ActiveAppearance(pub AppearanceSettings);

impl Global for ActiveAppearance {}

/// The active settings (defaults before any are set).
pub fn current(cx: &App) -> AppearanceSettings {
    cx.try_global::<ActiveAppearance>()
        .map(|a| a.0.clone())
        .unwrap_or_default()
}

/// The active zoom.
pub fn zoom(cx: &App) -> f32 {
    cx.try_global::<ActiveAppearance>()
        .map_or(1.0, |a| a.0.zoom)
}

/// Store `s` (sanitized) as the active settings. The caller restyles and saves.
pub fn set(s: AppearanceSettings, cx: &mut App) {
    cx.set_global(ActiveAppearance(s.sanitized()));
}

/// The UI font size at the active zoom (gpui-component's `font_size`, the rem size).
pub fn ui_font_size(cx: &App) -> Pixels {
    px(UI_FONT * zoom(cx))
}

/// The editor font family.
pub fn editor_font_family(cx: &App) -> SharedString {
    cx.try_global::<ActiveAppearance>()
        .map_or_else(|| MONO.into(), |a| a.0.editor_font_family.clone().into())
}

/// The editor font size at the active zoom.
pub fn editor_font_size(cx: &App) -> Pixels {
    let s = current(cx);
    px(s.editor_font_size * s.zoom)
}

/// A px value from the 100 % design, scaled by the active zoom.
pub fn scaled(v: f32, cx: &App) -> Pixels {
    px(v * zoom(cx))
}

/// Result-grid row height at zoom `z`, whole pixels.
pub fn grid_row_height(z: f32) -> f32 {
    (GRID_ROW * clamp_zoom(z)).round()
}

/// The table size for result grids: the compact design at 100 %, rows scaled otherwise.
pub fn table_size(cx: &App) -> Size {
    let z = zoom(cx);
    if (z - 1.0).abs() < 0.001 {
        Size::XSmall
    } else {
        Size::Size(px(grid_row_height(z)))
    }
}

/// Terminal font: family, size and line height (px) for the active settings.
#[derive(Clone, Debug, PartialEq)]
pub struct TermMetrics {
    /// Font family.
    pub family: SharedString,
    /// Font size, px.
    pub font_size: f32,
    /// Line height, whole px.
    pub line_height: f32,
}

/// Terminal metrics for `s`.
pub fn term_metrics_for(s: &AppearanceSettings) -> TermMetrics {
    let font_size = s.editor_font_size * s.zoom;
    TermMetrics {
        family: s.editor_font_family.clone().into(),
        font_size,
        line_height: (font_size * TERM_LINE_RATIO).round().max(1.0),
    }
}

/// Terminal metrics for the active settings.
pub fn term_metrics(cx: &App) -> TermMetrics {
    term_metrics_for(&current(cx))
}

/// How many font choices Settings → Appearance offers.
const MAX_FAMILIES: usize = 10;

/// Monospace-looking families from `installed`: the bundled font first, then the
/// current choice, then installed ones whose names say monospace, sorted, deduplicated.
pub fn mono_choices(installed: &[String], current: &str) -> Vec<String> {
    const HINTS: [&str; 10] = [
        "mono",
        "code",
        "consol",
        "menlo",
        "courier",
        "monaco",
        "hack",
        "inconsolata",
        "fira",
        "iosevka",
    ];
    let mut out = vec![MONO.to_owned()];
    if !current.is_empty() && current != MONO {
        out.push(current.to_owned());
    }
    let mut found: Vec<&String> = installed
        .iter()
        .filter(|n| {
            let l = n.to_lowercase();
            HINTS.iter().any(|h| l.contains(h))
        })
        .collect();
    found.sort();
    for n in found {
        if out.len() >= MAX_FAMILIES {
            break;
        }
        if !out.iter().any(|o| o == n) {
            out.push(n.clone());
        }
    }
    out
}

/// Installed font names, read once (enumerating fonts is slow).
pub fn installed_fonts(cx: &App) -> &'static [String] {
    static FONTS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    FONTS.get_or_init(|| cx.text_system().all_font_names())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_is_clamped() {
        assert_eq!(clamp_zoom(0.1), ZOOM_MIN);
        assert_eq!(clamp_zoom(5.0), ZOOM_MAX);
        assert_eq!(clamp_zoom(f32::NAN), 1.0);
        assert_eq!(clamp_zoom(1.3), 1.3);
    }

    #[test]
    fn zoom_steps_up_and_down() {
        assert_eq!(zoom_in(1.0), 1.1);
        assert_eq!(zoom_out(1.0), 0.9);
        assert_eq!(zoom_in(1.3), 1.5);
        assert_eq!(zoom_out(1.3), 1.25);
        assert_eq!(zoom_in(ZOOM_MAX), ZOOM_MAX);
        assert_eq!(zoom_out(ZOOM_MIN), ZOOM_MIN);
        assert_eq!(zoom_in(9.0), ZOOM_MAX);
        // Walking up from the minimum reaches the maximum and stays there.
        let mut z = ZOOM_MIN;
        for _ in 0..20 {
            z = zoom_in(z);
        }
        assert_eq!(z, ZOOM_MAX);
        let mut z = ZOOM_MAX;
        for _ in 0..20 {
            z = zoom_out(z);
        }
        assert_eq!(z, ZOOM_MIN);
        assert_eq!(percent(1.25), 125);
    }

    #[test]
    fn settings_round_trip() {
        let s = AppearanceSettings {
            editor_font_family: "JetBrains Mono".into(),
            editor_font_size: 14.0,
            zoom: 1.25,
        };
        let v = serde_json::to_value(&s).unwrap();
        let back: AppearanceSettings = serde_json::from_value(v).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn missing_or_bad_fields_fall_back() {
        let s: AppearanceSettings =
            serde_json::from_value(serde_json::json!({ "zoom": 9.0 })).unwrap();
        let s = s.sanitized();
        assert_eq!(s.zoom, ZOOM_MAX);
        assert_eq!(s.editor_font_family, MONO);
        assert_eq!(s.editor_font_size, 12.5);
        let s = AppearanceSettings {
            editor_font_family: "  ".into(),
            editor_font_size: 100.0,
            zoom: 0.0,
        }
        .sanitized();
        assert_eq!(s.editor_font_family, MONO);
        assert_eq!(s.editor_font_size, EDITOR_FONT_MAX);
        assert_eq!(s.zoom, ZOOM_MIN);
    }

    #[test]
    fn metrics_scale_with_zoom() {
        let base = AppearanceSettings::default();
        let m = term_metrics_for(&base);
        assert_eq!((m.font_size, m.line_height), (12.5, 19.0));
        let big = AppearanceSettings {
            zoom: 2.0,
            ..base.clone()
        };
        let m = term_metrics_for(&big);
        assert_eq!((m.font_size, m.line_height), (25.0, 38.0));
        assert_eq!(grid_row_height(1.0), 26.0);
        assert_eq!(grid_row_height(1.5), 39.0);
        assert_eq!(grid_row_height(0.7), 18.0);
    }

    #[test]
    fn mono_choices_put_the_bundled_font_first() {
        let installed: Vec<String> = [
            "Arial",
            "JetBrains Mono",
            "Fira Code",
            "Geist Mono",
            "Menlo",
        ]
        .map(String::from)
        .to_vec();
        let c = mono_choices(&installed, "Menlo");
        assert_eq!(c, ["Geist Mono", "Menlo", "Fira Code", "JetBrains Mono"]);
        assert_eq!(mono_choices(&[], ""), [MONO]);
    }
}
