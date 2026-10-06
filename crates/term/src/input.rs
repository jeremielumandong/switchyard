//! Encoding of keys, mouse events and pastes into the bytes a terminal program expects
//! (xterm conventions). Kept free of UI types so it can be tested directly.

use crate::terminal::Modes;

/// A key, independent of the UI toolkit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    /// Printable text (already shifted / composed).
    Char(char),
    /// Enter / Return.
    Enter,
    /// Tab.
    Tab,
    /// Backspace.
    Backspace,
    /// Escape.
    Escape,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Insert.
    Insert,
    /// Delete (forward).
    Delete,
    /// Function key F1–F12.
    F(u8),
}

/// Modifier keys.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    /// Shift.
    pub shift: bool,
    /// Alt / Option.
    pub alt: bool,
    /// Control.
    pub ctrl: bool,
}

impl Mods {
    fn param(self) -> u8 {
        1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl)
    }

    fn any(self) -> bool {
        self.shift || self.alt || self.ctrl
    }
}

fn csi_mod(final_byte: char, mods: Mods) -> Vec<u8> {
    if mods.any() {
        format!("\x1b[1;{}{final_byte}", mods.param()).into_bytes()
    } else {
        format!("\x1b[{final_byte}").into_bytes()
    }
}

fn tilde(code: u8, mods: Mods) -> Vec<u8> {
    if mods.any() {
        format!("\x1b[{code};{}~", mods.param()).into_bytes()
    } else {
        format!("\x1b[{code}~").into_bytes()
    }
}

/// Bytes for a key press, or `None` when the key sends nothing.
pub fn encode_key(key: &Key, mods: Mods, modes: Modes) -> Option<Vec<u8>> {
    let esc_prefix = |mut v: Vec<u8>| {
        if mods.alt {
            v.insert(0, 0x1b);
        }
        v
    };
    Some(match key {
        Key::Char(c) => {
            if mods.ctrl {
                let b = match c.to_ascii_lowercase() {
                    c @ 'a'..='z' => c as u8 - b'a' + 1,
                    '@' | ' ' | '2' => 0,
                    '[' | '3' => 0x1b,
                    '\\' | '4' => 0x1c,
                    ']' | '5' => 0x1d,
                    '^' | '6' => 0x1e,
                    '_' | '-' | '7' => 0x1f,
                    '?' | '8' => 0x7f,
                    _ => {
                        let mut buf = [0u8; 4];
                        return Some(esc_prefix(c.encode_utf8(&mut buf).as_bytes().to_vec()));
                    }
                };
                esc_prefix(vec![b])
            } else {
                let mut buf = [0u8; 4];
                esc_prefix(c.encode_utf8(&mut buf).as_bytes().to_vec())
            }
        }
        Key::Enter => esc_prefix(vec![b'\r']),
        Key::Tab if mods.shift => b"\x1b[Z".to_vec(),
        Key::Tab => esc_prefix(vec![b'\t']),
        Key::Backspace if mods.ctrl => esc_prefix(vec![0x08]),
        Key::Backspace => esc_prefix(vec![0x7f]),
        Key::Escape => esc_prefix(vec![0x1b]),
        Key::Up | Key::Down | Key::Right | Key::Left | Key::Home | Key::End => {
            let f = match key {
                Key::Up => 'A',
                Key::Down => 'B',
                Key::Right => 'C',
                Key::Left => 'D',
                Key::Home => 'H',
                _ => 'F',
            };
            if modes.app_cursor && !mods.any() {
                format!("\x1bO{f}").into_bytes()
            } else {
                csi_mod(f, mods)
            }
        }
        Key::Insert => tilde(2, mods),
        Key::Delete => tilde(3, mods),
        Key::PageUp => tilde(5, mods),
        Key::PageDown => tilde(6, mods),
        Key::F(n @ 1..=4) => {
            let f = (b'P' + n - 1) as char;
            if mods.any() {
                csi_mod(f, mods)
            } else {
                format!("\x1bO{f}").into_bytes()
            }
        }
        Key::F(n @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][(*n - 5) as usize];
            tilde(code, mods)
        }
        Key::F(_) => return None,
    })
}

/// Text to send for a paste: bracketed when the program asked for it, with line endings
/// normalized to carriage returns. Escape characters are removed from bracketed pastes
/// so pasted text cannot end the bracket early.
pub fn encode_paste(text: &str, modes: Modes) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    if modes.bracketed_paste {
        let mut out = b"\x1b[200~".to_vec();
        out.extend(normalized.bytes().filter(|b| *b != 0x1b));
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        normalized.into_bytes()
    }
}

/// Mouse buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    /// Left.
    Left,
    /// Middle.
    Middle,
    /// Right.
    Right,
    /// Wheel up.
    WheelUp,
    /// Wheel down.
    WheelDown,
}

/// What happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    /// Button pressed (or wheel notch).
    Press,
    /// Button released.
    Release,
    /// Pointer moved; `Some` while a button is held.
    Move(Option<MouseButton>),
}

/// Bytes reporting a mouse event at a 0-based cell, or `None` when the program did not
/// ask for this kind of report.
pub fn encode_mouse(
    action: MouseAction,
    button: MouseButton,
    col: u16,
    row: u16,
    mods: Mods,
    modes: Modes,
) -> Option<Vec<u8>> {
    if !modes.mouse {
        return None;
    }
    let (base, motion) = match action {
        MouseAction::Move(Some(b)) if modes.mouse_drag || modes.mouse_motion => (b, true),
        MouseAction::Move(None) if modes.mouse_motion => (MouseButton::Left, true),
        MouseAction::Move(_) => return None,
        _ => (button, false),
    };
    let mut code: u8 = match base {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
    };
    if matches!(action, MouseAction::Move(None)) {
        code = 3;
    }
    if motion {
        code += 32;
    }
    code += 4 * u8::from(mods.shift) + 8 * u8::from(mods.alt) + 16 * u8::from(mods.ctrl);
    let release = action == MouseAction::Release;
    if modes.sgr_mouse {
        let end = if release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{};{}{end}", col + 1, row + 1).into_bytes());
    }
    if release {
        code = 3 + (code & !3);
    }
    // Legacy encoding cannot express positions past 222.
    if col > 222 || row > 222 {
        return None;
    }
    Some(vec![
        0x1b,
        b'[',
        b'M',
        32 + code,
        32 + col as u8 + 1,
        32 + row as u8 + 1,
    ])
}

/// Bytes for focus gained / lost when the program enabled focus reporting.
pub fn encode_focus(focused: bool, modes: Modes) -> Option<&'static [u8]> {
    modes
        .focus_events
        .then_some(if focused { b"\x1b[I" } else { b"\x1b[O" })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(shift: bool, alt: bool, ctrl: bool) -> Mods {
        Mods { shift, alt, ctrl }
    }

    #[test]
    fn keys() {
        let n = Modes::default();
        let app = Modes {
            app_cursor: true,
            ..Modes::default()
        };
        let enc =
            |k: Key, mods: Mods, modes: Modes| encode_key(&k, mods, modes).unwrap_or_default();
        assert_eq!(enc(Key::Char('a'), Mods::default(), n), b"a");
        assert_eq!(enc(Key::Char('é'), Mods::default(), n), "é".as_bytes());
        assert_eq!(enc(Key::Char('c'), m(false, false, true), n), [3]);
        assert_eq!(enc(Key::Char('['), m(false, false, true), n), [0x1b]);
        assert_eq!(enc(Key::Char('x'), m(false, true, false), n), b"\x1bx");
        assert_eq!(enc(Key::Enter, Mods::default(), n), b"\r");
        assert_eq!(enc(Key::Backspace, Mods::default(), n), [0x7f]);
        assert_eq!(enc(Key::Tab, m(true, false, false), n), b"\x1b[Z");
        assert_eq!(enc(Key::Up, Mods::default(), n), b"\x1b[A");
        assert_eq!(enc(Key::Up, Mods::default(), app), b"\x1bOA");
        assert_eq!(enc(Key::Right, m(false, false, true), app), b"\x1b[1;5C");
        assert_eq!(enc(Key::Delete, Mods::default(), n), b"\x1b[3~");
        assert_eq!(enc(Key::PageUp, m(true, false, false), n), b"\x1b[5;2~");
        assert_eq!(enc(Key::F(1), Mods::default(), n), b"\x1bOP");
        assert_eq!(enc(Key::F(5), Mods::default(), n), b"\x1b[15~");
        assert_eq!(enc(Key::F(12), Mods::default(), n), b"\x1b[24~");
    }

    #[test]
    fn paste() {
        let plain = Modes::default();
        let bracketed = Modes {
            bracketed_paste: true,
            ..Modes::default()
        };
        assert_eq!(encode_paste("a\nb\r\nc", plain), b"a\rb\rc");
        assert_eq!(
            encode_paste("x\x1b[201~y", bracketed),
            b"\x1b[200~x[201~y\x1b[201~"
        );
    }

    #[test]
    fn mouse() {
        let off = Modes::default();
        assert!(
            encode_mouse(
                MouseAction::Press,
                MouseButton::Left,
                0,
                0,
                Mods::default(),
                off
            )
            .is_none()
        );
        let sgr = Modes {
            mouse: true,
            sgr_mouse: true,
            ..Modes::default()
        };
        assert_eq!(
            encode_mouse(
                MouseAction::Press,
                MouseButton::Left,
                4,
                9,
                Mods::default(),
                sgr
            ),
            Some(b"\x1b[<0;5;10M".to_vec())
        );
        assert_eq!(
            encode_mouse(
                MouseAction::Release,
                MouseButton::Left,
                4,
                9,
                Mods::default(),
                sgr
            ),
            Some(b"\x1b[<0;5;10m".to_vec())
        );
        assert_eq!(
            encode_mouse(
                MouseAction::Press,
                MouseButton::WheelUp,
                0,
                0,
                Mods::default(),
                sgr
            ),
            Some(b"\x1b[<64;1;1M".to_vec())
        );
        assert!(
            encode_mouse(
                MouseAction::Move(Some(MouseButton::Left)),
                MouseButton::Left,
                1,
                1,
                Mods::default(),
                sgr
            )
            .is_none(),
            "drag reports need 1002"
        );
        let legacy = Modes {
            mouse: true,
            ..Modes::default()
        };
        assert_eq!(
            encode_mouse(
                MouseAction::Press,
                MouseButton::Right,
                0,
                0,
                Mods::default(),
                legacy
            ),
            Some(vec![0x1b, b'[', b'M', 34, 33, 33])
        );
        assert_eq!(
            encode_mouse(
                MouseAction::Release,
                MouseButton::Right,
                0,
                0,
                Mods::default(),
                legacy
            ),
            Some(vec![0x1b, b'[', b'M', 35, 33, 33])
        );
    }
}
