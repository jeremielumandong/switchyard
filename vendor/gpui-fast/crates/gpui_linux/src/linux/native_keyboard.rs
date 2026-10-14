//! Original per-event XKB facts; no shortcut/display-key inference.

#[path = "native_keyboard/codes.rs"]
mod codes;

use gpui::{Modifiers, NativeKeyCode, NativeKeyMetadata, SharedString};
use xkbcommon::xkb::{self, Keycode, State};

fn character(symbol: xkb::Keysym) -> Option<SharedString> {
    let text = xkb::keysym_to_utf8(symbol);
    let mut chars = text.chars();
    let first = chars.next()?;
    (!first.is_control() && chars.next().is_none()).then(|| text.into())
}

pub(super) fn metadata(
    state: &State,
    modifiers: Modifiers,
    code: Keycode,
    composing: bool,
) -> NativeKeyMetadata {
    let keymap = state.get_keymap();
    let layout_character = match keymap.key_get_syms_by_level(code, state.key_get_layout(code), 0) {
        [symbol] => character(*symbol),
        _ => None,
    };
    let symbol = state.key_get_one_sym(code);
    NativeKeyMetadata {
        physical_code: NativeKeyCode::Xkb(code.raw()),
        code: codes::dom_code_for_xkb(code.raw()).map(Into::into),
        key_symbol: Some(symbol.raw()),
        key_character: character(symbol),
        layout_character,
        modifiers,
        caps_lock: Some(state.mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_EFFECTIVE)),
        num_lock: Some(state.mod_name_is_active(xkb::MOD_NAME_NUM, xkb::STATE_MODS_EFFECTIVE)),
        is_composing: Some(composing),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(layout: &str) -> State {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "pc105",
            layout,
            "",
            None,
            xkb::COMPILE_NO_FLAGS,
        )
        .expect("actual fixture XKB layout");
        State::new(&keymap)
    }

    #[test]
    fn physical_code_does_not_follow_layout_or_control_character() {
        let us = state("us");
        let de = state("de");
        let code = Keycode::new(29); // Original physical US Y key.
        let a = metadata(&us, Modifiers::default(), code, false);
        let b = metadata(&de, Modifiers::default(), code, false);
        assert_eq!(a.physical_code, NativeKeyCode::Xkb(29));
        assert_eq!(a.code.as_deref(), Some("KeyY"));
        assert_eq!(b.code, a.code);
        assert_eq!(a.key_character.as_deref(), Some("y"));
        assert_eq!(b.key_character.as_deref(), Some("z"));
        assert_eq!(b.layout_character.as_deref(), Some("z"));

        let mut ctrl = state("us");
        ctrl.update_key(Keycode::new(37), xkb::KeyDirection::Down);
        assert_eq!(ctrl.key_get_utf8(Keycode::new(54)), "\u{3}");
        let captured = metadata(&ctrl, Modifiers::control(), Keycode::new(54), false);
        assert_eq!(captured.key_character.as_deref(), Some("c"));
        assert!(captured.modifiers.control);
    }

    #[test]
    fn actual_locks_shift_layout_unicode_and_unknown_codes_remain_distinct() {
        let mut shifted = state("us");
        shifted.update_key(Keycode::new(50), xkb::KeyDirection::Down);
        let captured = metadata(&shifted, Modifiers::shift(), Keycode::new(10), false);
        assert_eq!(captured.key_character.as_deref(), Some("!"));
        assert_eq!(captured.layout_character.as_deref(), Some("1"));
        assert_eq!(captured.caps_lock, Some(false));
        assert_eq!(captured.num_lock, Some(false));

        let mut locked = state("us");
        locked.update_key(Keycode::new(66), xkb::KeyDirection::Down);
        locked.update_key(Keycode::new(66), xkb::KeyDirection::Up);
        locked.update_key(Keycode::new(77), xkb::KeyDirection::Down);
        locked.update_key(Keycode::new(77), xkb::KeyDirection::Up);
        let captured = metadata(&locked, Modifiers::default(), Keycode::new(38), true);
        assert_eq!(captured.key_character.as_deref(), Some("A"));
        assert_eq!(captured.layout_character.as_deref(), Some("a"));
        assert_eq!(captured.caps_lock, Some(true));
        assert_eq!(captured.num_lock, Some(true));
        assert_eq!(captured.is_composing, Some(true));
        for (code, expected) in [
            (50, "ShiftLeft"),
            (62, "ShiftRight"),
            (37, "ControlLeft"),
            (105, "ControlRight"),
            (64, "AltLeft"),
            (108, "AltRight"),
            (90, "Numpad0"),
        ] {
            assert_eq!(
                metadata(&locked, Modifiers::default(), Keycode::new(code), false)
                    .code
                    .as_deref(),
                Some(expected)
            );
        }
        assert_eq!(
            metadata(&locked, Modifiers::default(), Keycode::new(0), false).code,
            None
        );
        let ru = state("ru");
        let captured = metadata(&ru, Modifiers::default(), Keycode::new(38), false);
        assert_eq!(captured.code.as_deref(), Some("KeyA"));
        assert_eq!(captured.key_character.as_deref(), Some("ф"));
    }

    #[test]
    fn generated_mapping_matches_all_original_native_rows() {
        // Independent parse of the retained original data, not the generated Rust table.
        let source = include_str!("native_keyboard/dom_code_data.inc")
            .lines()
            .map(|line| line.split("//").next().unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        let mut seen = std::collections::HashSet::new();
        for tail in source.split("DOM_CODE(").skip(1) {
            let fields = tail
                .split(')')
                .next()
                .unwrap()
                .split(',')
                .map(str::trim)
                .collect::<Vec<_>>();
            if fields.len() < 7 || !fields[0].starts_with("0x") {
                continue;
            }
            let native = u32::from_str_radix(fields[2].trim_start_matches("0x"), 16).unwrap();
            if native == 0 || !seen.insert(native) {
                continue;
            }
            let expected = fields[5]
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'));
            assert_eq!(codes::dom_code_for_xkb(native), expected, "{native}");
        }
        assert_eq!(seen.len(), 225);
        assert_eq!(codes::dom_code_for_xkb(u32::MAX), None);
    }
}
