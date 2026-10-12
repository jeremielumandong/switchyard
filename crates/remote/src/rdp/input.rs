//! Keyboard and mouse input for the in-app RDP client, independent of the UI toolkit.
//!
//! Keys are sent as scan codes (set 1, US positions) so shortcuts work whatever the
//! server's layout; plain typed text (no Ctrl, Alt or Meta held) goes as Unicode key
//! events instead, so it comes out right on any server layout. Modifiers are sent as
//! their own key presses when they change.

/// A keyboard scan code (set 1), with the `E0` prefix flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Scancode {
    /// `E0`-prefixed (arrows, right Ctrl / Alt, Windows keys, …).
    pub extended: bool,
    /// The code.
    pub code: u8,
}

impl Scancode {
    const fn plain(code: u8) -> Self {
        Self {
            extended: false,
            code,
        }
    }

    const fn ext(code: u8) -> Self {
        Self {
            extended: true,
            code,
        }
    }
}

/// Left Ctrl.
pub const LCTRL: Scancode = Scancode::plain(0x1D);
/// Left Alt.
pub const LALT: Scancode = Scancode::plain(0x38);
/// Left Shift.
pub const LSHIFT: Scancode = Scancode::plain(0x2A);
/// Left Windows key.
pub const LWIN: Scancode = Scancode::ext(0x5B);
/// Delete (the navigation key).
pub const DELETE: Scancode = Scancode::ext(0x53);

/// A mouse button.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseButton {
    /// Left.
    Left,
    /// Middle (wheel).
    Middle,
    /// Right.
    Right,
    /// Back.
    X1,
    /// Forward.
    X2,
}

/// One input event for the remote session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RdpInput {
    /// A key by scan code.
    Key {
        /// Which key.
        code: Scancode,
        /// Pressed (`true`) or released.
        down: bool,
    },
    /// A typed character (layout independent).
    Text {
        /// The character.
        ch: char,
        /// Pressed (`true`) or released.
        down: bool,
    },
    /// The pointer moved to this desktop position.
    MouseMove {
        /// X in desktop pixels.
        x: u16,
        /// Y in desktop pixels.
        y: u16,
    },
    /// A mouse button.
    Button {
        /// Which button.
        button: MouseButton,
        /// Pressed (`true`) or released.
        down: bool,
    },
    /// Wheel rotation in units of 1/120 notch; positive is up (or right).
    Wheel {
        /// Vertical (`true`) or horizontal.
        vertical: bool,
        /// Rotation units.
        units: i16,
    },
    /// Release every key and button still held (focus lost, tab hidden).
    ReleaseAll,
}

/// Which modifiers are held, as the UI reports them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyMods {
    /// Ctrl.
    pub ctrl: bool,
    /// Alt / Option.
    pub alt: bool,
    /// Shift.
    pub shift: bool,
    /// Command (macOS) / Windows / Super: sent as the Windows key.
    pub meta: bool,
}

/// The scan code of a key, by the UI's key name (`a`, `enter`, `f5`, `left`, …).
/// Shifted symbols map to their key (`!` is the `1` key). `None` for keys RDP has no
/// code for here.
pub fn scancode(key: &str) -> Option<Scancode> {
    let k = key.to_ascii_lowercase();
    let sc = match k.as_str() {
        "escape" | "esc" => Scancode::plain(0x01),
        "1" | "!" => Scancode::plain(0x02),
        "2" | "@" => Scancode::plain(0x03),
        "3" | "#" => Scancode::plain(0x04),
        "4" | "$" => Scancode::plain(0x05),
        "5" | "%" => Scancode::plain(0x06),
        "6" | "^" => Scancode::plain(0x07),
        "7" | "&" => Scancode::plain(0x08),
        "8" | "*" => Scancode::plain(0x09),
        "9" | "(" => Scancode::plain(0x0A),
        "0" | ")" => Scancode::plain(0x0B),
        "-" | "_" => Scancode::plain(0x0C),
        "=" | "+" => Scancode::plain(0x0D),
        "backspace" => Scancode::plain(0x0E),
        "tab" => Scancode::plain(0x0F),
        "q" => Scancode::plain(0x10),
        "w" => Scancode::plain(0x11),
        "e" => Scancode::plain(0x12),
        "r" => Scancode::plain(0x13),
        "t" => Scancode::plain(0x14),
        "y" => Scancode::plain(0x15),
        "u" => Scancode::plain(0x16),
        "i" => Scancode::plain(0x17),
        "o" => Scancode::plain(0x18),
        "p" => Scancode::plain(0x19),
        "[" | "{" => Scancode::plain(0x1A),
        "]" | "}" => Scancode::plain(0x1B),
        "enter" | "return" => Scancode::plain(0x1C),
        "control" | "ctrl" => LCTRL,
        "a" => Scancode::plain(0x1E),
        "s" => Scancode::plain(0x1F),
        "d" => Scancode::plain(0x20),
        "f" => Scancode::plain(0x21),
        "g" => Scancode::plain(0x22),
        "h" => Scancode::plain(0x23),
        "j" => Scancode::plain(0x24),
        "k" => Scancode::plain(0x25),
        "l" => Scancode::plain(0x26),
        ";" | ":" => Scancode::plain(0x27),
        "'" | "\"" => Scancode::plain(0x28),
        "`" | "~" => Scancode::plain(0x29),
        "shift" => LSHIFT,
        "\\" | "|" => Scancode::plain(0x2B),
        "z" => Scancode::plain(0x2C),
        "x" => Scancode::plain(0x2D),
        "c" => Scancode::plain(0x2E),
        "v" => Scancode::plain(0x2F),
        "b" => Scancode::plain(0x30),
        "n" => Scancode::plain(0x31),
        "m" => Scancode::plain(0x32),
        "," | "<" => Scancode::plain(0x33),
        "." | ">" => Scancode::plain(0x34),
        "/" | "?" => Scancode::plain(0x35),
        "alt" | "option" => LALT,
        "space" | " " => Scancode::plain(0x39),
        "capslock" | "caps_lock" => Scancode::plain(0x3A),
        "f1" => Scancode::plain(0x3B),
        "f2" => Scancode::plain(0x3C),
        "f3" => Scancode::plain(0x3D),
        "f4" => Scancode::plain(0x3E),
        "f5" => Scancode::plain(0x3F),
        "f6" => Scancode::plain(0x40),
        "f7" => Scancode::plain(0x41),
        "f8" => Scancode::plain(0x42),
        "f9" => Scancode::plain(0x43),
        "f10" => Scancode::plain(0x44),
        "f11" => Scancode::plain(0x57),
        "f12" => Scancode::plain(0x58),
        "home" => Scancode::ext(0x47),
        "up" => Scancode::ext(0x48),
        "pageup" => Scancode::ext(0x49),
        "left" => Scancode::ext(0x4B),
        "right" => Scancode::ext(0x4D),
        "end" => Scancode::ext(0x4F),
        "down" => Scancode::ext(0x50),
        "pagedown" => Scancode::ext(0x51),
        "insert" => Scancode::ext(0x52),
        "delete" => DELETE,
        "platform" | "cmd" | "super" | "win" | "meta" => LWIN,
        "menu" | "contextmenu" => Scancode::ext(0x5D),
        _ => return None,
    };
    Some(sc)
}

/// Whether `key` names a modifier (sent from [`modifier_changes`], not as a key).
pub fn is_modifier(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "shift" | "control" | "ctrl" | "alt" | "option" | "platform" | "cmd" | "super" | "meta"
    )
}

/// The typed character of a key press, when it should go as text: one printable
/// character and no Ctrl, Alt or Meta held (Shift is fine; it is already in the char).
fn typed_char(key_char: Option<&str>, mods: KeyMods) -> Option<char> {
    if mods.ctrl || mods.alt || mods.meta {
        return None;
    }
    let mut chars = key_char?.chars();
    let ch = chars.next()?;
    (chars.next().is_none() && !ch.is_control()).then_some(ch)
}

/// What a key press sends: a Unicode press and release for typed text, else the key's
/// scan code down. Nothing for modifiers and unknown keys.
pub fn key_down(key: &str, key_char: Option<&str>, mods: KeyMods) -> Vec<RdpInput> {
    if is_modifier(key) {
        return Vec::new();
    }
    // Space, Enter and Tab go as keys: some remote apps watch the key, not the text.
    let named = matches!(key, "space" | "enter" | "tab" | "backspace" | "escape");
    if !named && let Some(ch) = typed_char(key_char, mods) {
        return vec![
            RdpInput::Text { ch, down: true },
            RdpInput::Text { ch, down: false },
        ];
    }
    scancode(key)
        .map(|code| vec![RdpInput::Key { code, down: true }])
        .unwrap_or_default()
}

/// What a key release sends: the scan code up (typed text was released on press).
pub fn key_up(key: &str, key_char: Option<&str>, mods: KeyMods) -> Vec<RdpInput> {
    if is_modifier(key) {
        return Vec::new();
    }
    let named = matches!(key, "space" | "enter" | "tab" | "backspace" | "escape");
    if !named && typed_char(key_char, mods).is_some() {
        return Vec::new();
    }
    scancode(key)
        .map(|code| vec![RdpInput::Key { code, down: false }])
        .unwrap_or_default()
}

/// The modifier presses and releases that take the remote side from `old` to `new`.
pub fn modifier_changes(old: KeyMods, new: KeyMods) -> Vec<RdpInput> {
    let mut out = Vec::new();
    for (was, now, code) in [
        (old.ctrl, new.ctrl, LCTRL),
        (old.alt, new.alt, LALT),
        (old.shift, new.shift, LSHIFT),
        (old.meta, new.meta, LWIN),
    ] {
        if was != now {
            out.push(RdpInput::Key { code, down: now });
        }
    }
    out
}

/// Ctrl+Alt+Del: the secure attention sequence, then everything released.
pub fn ctrl_alt_del() -> Vec<RdpInput> {
    let mut out: Vec<RdpInput> = [LCTRL, LALT, DELETE]
        .into_iter()
        .map(|code| RdpInput::Key { code, down: true })
        .collect();
    out.extend(
        [DELETE, LALT, LCTRL]
            .into_iter()
            .map(|code| RdpInput::Key { code, down: false }),
    );
    out
}

/// How the remote desktop is drawn in the tab.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScaleMode {
    /// Scaled to fit the tab, keeping the aspect ratio, centred.
    #[default]
    Fit,
    /// One desktop pixel per screen pixel, centred (clipped when larger).
    Actual,
}

/// Where the desktop image goes in the view, in view coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    /// Left.
    pub x: f32,
    /// Top.
    pub y: f32,
    /// Drawn width.
    pub w: f32,
    /// Drawn height.
    pub h: f32,
}

/// Lay a `remote_w` × `remote_h` desktop out in a `view_w` × `view_h` view.
pub fn place(view_w: f32, view_h: f32, remote_w: u16, remote_h: u16, mode: ScaleMode) -> Placement {
    let (rw, rh) = (f32::from(remote_w.max(1)), f32::from(remote_h.max(1)));
    let scale = match mode {
        ScaleMode::Fit => (view_w / rw).min(view_h / rh).max(0.01),
        ScaleMode::Actual => 1.0,
    };
    let (w, h) = (rw * scale, rh * scale);
    Placement {
        x: (view_w - w) / 2.0,
        y: (view_h - h) / 2.0,
        w,
        h,
    }
}

/// The desktop pixel under view point (`x`, `y`), clamped to the desktop (a drag that
/// leaves the image keeps reporting its edge).
pub fn to_remote(p: Placement, x: f32, y: f32, remote_w: u16, remote_h: u16) -> (u16, u16) {
    let fx = if p.w > 0.0 { (x - p.x) / p.w } else { 0.0 };
    let fy = if p.h > 0.0 { (y - p.y) / p.h } else { 0.0 };
    let max_x = f32::from(remote_w.saturating_sub(1));
    let max_y = f32::from(remote_h.saturating_sub(1));
    // Clamped to the desktop first, so the casts cannot wrap.
    (
        (fx * f32::from(remote_w)).floor().clamp(0.0, max_x) as u16,
        (fy * f32::from(remote_h)).floor().clamp(0.0, max_y) as u16,
    )
}

/// Wheel units (1/120 notch) for a scroll of `lines` lines; positive is up.
pub fn wheel_units(lines: f32) -> i16 {
    // Clamped to i16's range first, so the cast cannot wrap.
    (lines * 120.0).round().clamp(-32_000.0, 32_000.0) as i16
}

/// The desktop size to ask for: within 200…8192 and the width a multiple of 4 (some
/// servers refuse other widths).
pub fn desktop_size(width: u32, height: u32) -> (u16, u16) {
    let w = width.clamp(200, 8192) & !3;
    let h = height.clamp(200, 8192);
    (
        u16::try_from(w).unwrap_or(8192),
        u16::try_from(h).unwrap_or(8192),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_navigation_keys_have_set_1_codes() {
        assert_eq!(scancode("a"), Some(Scancode::plain(0x1E)));
        assert_eq!(scancode("A"), Some(Scancode::plain(0x1E)));
        assert_eq!(scancode("z"), Some(Scancode::plain(0x2C)));
        assert_eq!(scancode("1"), Some(Scancode::plain(0x02)));
        assert_eq!(scancode("!"), scancode("1"), "shifted symbol: same key");
        assert_eq!(scancode("0"), Some(Scancode::plain(0x0B)));
        assert_eq!(scancode("enter"), Some(Scancode::plain(0x1C)));
        assert_eq!(scancode("f1"), Some(Scancode::plain(0x3B)));
        assert_eq!(scancode("f12"), Some(Scancode::plain(0x58)));
        assert_eq!(scancode("left"), Some(Scancode::ext(0x4B)));
        assert_eq!(scancode("delete"), Some(DELETE));
        assert_eq!(scancode("platform"), Some(LWIN));
        assert_eq!(scancode("hyper"), None);
    }

    #[test]
    fn typed_text_goes_as_unicode_and_shortcuts_as_scan_codes() {
        let plain = KeyMods::default();
        assert_eq!(
            key_down("a", Some("a"), plain),
            vec![
                RdpInput::Text {
                    ch: 'a',
                    down: true
                },
                RdpInput::Text {
                    ch: 'a',
                    down: false
                },
            ]
        );
        assert!(key_up("a", Some("a"), plain).is_empty());
        // Shift is in the character already.
        let shift = KeyMods {
            shift: true,
            ..KeyMods::default()
        };
        assert_eq!(
            key_down("1", Some("!"), shift)[0],
            RdpInput::Text {
                ch: '!',
                down: true
            }
        );
        // Ctrl+C: the C key, held until released.
        let ctrl = KeyMods {
            ctrl: true,
            ..KeyMods::default()
        };
        let c = Scancode::plain(0x2E);
        assert_eq!(
            key_down("c", Some("c"), ctrl),
            vec![RdpInput::Key {
                code: c,
                down: true
            }]
        );
        assert_eq!(
            key_up("c", Some("c"), ctrl),
            vec![RdpInput::Key {
                code: c,
                down: false
            }]
        );
        // Named keys stay keys even though they produce a character.
        assert_eq!(
            key_down("enter", Some("\n"), plain),
            vec![RdpInput::Key {
                code: Scancode::plain(0x1C),
                down: true
            }]
        );
        assert_eq!(
            key_down("space", Some(" "), plain),
            vec![RdpInput::Key {
                code: Scancode::plain(0x39),
                down: true
            }]
        );
        // Modifiers alone send nothing here; unknown keys neither.
        assert!(key_down("shift", None, shift).is_empty());
        assert!(key_down("hyper", None, plain).is_empty());
    }

    #[test]
    fn modifier_changes_press_and_release_the_left_keys() {
        let old = KeyMods {
            ctrl: true,
            ..KeyMods::default()
        };
        let new = KeyMods {
            shift: true,
            meta: true,
            ..KeyMods::default()
        };
        assert_eq!(
            modifier_changes(old, new),
            vec![
                RdpInput::Key {
                    code: LCTRL,
                    down: false
                },
                RdpInput::Key {
                    code: LSHIFT,
                    down: true
                },
                RdpInput::Key {
                    code: LWIN,
                    down: true
                },
            ]
        );
        assert!(modifier_changes(new, new).is_empty());
    }

    #[test]
    fn ctrl_alt_del_presses_then_releases_in_reverse() {
        let seq = ctrl_alt_del();
        let codes: Vec<(u8, bool)> = seq
            .iter()
            .map(|i| match i {
                RdpInput::Key { code, down } => (code.code, *down),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            codes,
            vec![
                (0x1D, true),
                (0x38, true),
                (0x53, true),
                (0x53, false),
                (0x38, false),
                (0x1D, false)
            ]
        );
    }

    #[test]
    fn fit_letterboxes_and_maps_points_back() {
        // 1000×500 view, 800×600 desktop: height bound, scale 5/6.
        let p = place(1000.0, 500.0, 800, 600, ScaleMode::Fit);
        assert!((p.h - 500.0).abs() < 0.01);
        assert!((p.w - 666.666).abs() < 0.01);
        assert!((p.x - 166.666).abs() < 0.01);
        assert_eq!(p.y, 0.0);
        assert_eq!(to_remote(p, p.x, 0.0, 800, 600), (0, 0));
        assert_eq!(to_remote(p, p.x + p.w / 2.0, 250.0, 800, 600), (400, 300));
        // Outside the image: clamped to the edge.
        assert_eq!(to_remote(p, 0.0, 9999.0, 800, 600), (0, 599));
        assert_eq!(to_remote(p, 9999.0, -5.0, 800, 600), (799, 0));
        // Actual size: one to one, centred.
        let a = place(1000.0, 500.0, 800, 600, ScaleMode::Actual);
        assert_eq!((a.x, a.y, a.w, a.h), (100.0, -50.0, 800.0, 600.0));
        assert_eq!(to_remote(a, 110.0, 0.0, 800, 600), (10, 50));
    }

    #[test]
    fn wheel_and_desktop_size_are_clamped() {
        assert_eq!(wheel_units(1.0), 120);
        assert_eq!(wheel_units(-0.5), -60);
        assert_eq!(wheel_units(1e9), 32_000);
        assert_eq!(desktop_size(1366, 768), (1364, 768));
        assert_eq!(desktop_size(10, 10), (200, 200));
        assert_eq!(desktop_size(100_000, 100_000), (8192, 8192));
    }
}
