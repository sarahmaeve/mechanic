//! Keyboard input translation: winit KeyEvent → PTY byte sequences.

use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Translate key presses to PTY bytes; releases and unsupported keys return `None`.
/// `cursor_app_mode` selects SS3 sequences for arrows and Home/End.
pub fn translate_key(
    event: &KeyEvent,
    modifiers: ModifiersState,
    cursor_app_mode: bool,
) -> Option<Vec<u8>> {
    if event.state != ElementState::Pressed {
        return None;
    }

    match &event.logical_key {
        Key::Named(named) => {
            if let Some(bytes) = modified_named_key(named, modifiers) {
                return Some(bytes);
            }
            if let Some(bytes) = modified_arrow(named, modifiers, cursor_app_mode) {
                return Some(bytes);
            }
            if cursor_app_mode && let Some(bytes) = named_key_bytes_app_mode(named) {
                return Some(bytes);
            }
            named_key_bytes(named)
        }
        Key::Character(ch) => {
            // macOS may omit event.text for Ctrl combinations.
            if modifiers.control_key()
                && let Some(ctrl_byte) = ctrl_char(ch)
            {
                return Some(vec![ctrl_byte]);
            }

            if let Some(text) = &event.text
                && !text.is_empty()
            {
                return Some(text.as_bytes().to_vec());
            }
            if !ch.is_empty() {
                return Some(ch.as_bytes().to_vec());
            }
            None
        }
        _ => None,
    }
}

/// Convert a character to its ASCII control-character byte when Ctrl is held.
pub fn ctrl_char(ch: &str) -> Option<u8> {
    let c = ch.chars().next()?;
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '[' => Some(0x1B),
        '\\' => Some(0x1C),
        ']' => Some(0x1D),
        '^' => Some(0x1E),
        '_' => Some(0x1F),
        '@' => Some(0x00),
        _ => None,
    }
}

/// Map a `NamedKey` to its standard VT/ANSI escape sequence bytes.
pub fn named_key_bytes(key: &NamedKey) -> Option<Vec<u8>> {
    let seq: &[u8] = match key {
        NamedKey::Space => b" ",
        NamedKey::Enter => b"\r",
        NamedKey::Backspace => b"\x7f",
        NamedKey::Tab => b"\t",
        NamedKey::Escape => b"\x1b",

        NamedKey::ArrowUp => b"\x1b[A",
        NamedKey::ArrowDown => b"\x1b[B",
        NamedKey::ArrowRight => b"\x1b[C",
        NamedKey::ArrowLeft => b"\x1b[D",

        NamedKey::Home => b"\x1b[H",
        NamedKey::End => b"\x1b[F",
        NamedKey::PageUp => b"\x1b[5~",
        NamedKey::PageDown => b"\x1b[6~",
        NamedKey::Delete => b"\x1b[3~",
        NamedKey::Insert => b"\x1b[2~",

        NamedKey::F1 => b"\x1bOP",
        NamedKey::F2 => b"\x1bOQ",
        NamedKey::F3 => b"\x1bOR",
        NamedKey::F4 => b"\x1bOS",

        NamedKey::F5 => b"\x1b[15~",
        NamedKey::F6 => b"\x1b[17~",
        NamedKey::F7 => b"\x1b[18~",
        NamedKey::F8 => b"\x1b[19~",
        NamedKey::F9 => b"\x1b[20~",
        NamedKey::F10 => b"\x1b[21~",
        NamedKey::F11 => b"\x1b[23~",
        NamedKey::F12 => b"\x1b[24~",

        _ => return None,
    };

    Some(seq.to_vec())
}

/// Map the 6 keys that change in DECCKM application cursor mode (DECSET 1).
pub fn named_key_bytes_app_mode(key: &NamedKey) -> Option<Vec<u8>> {
    let seq: &[u8] = match key {
        NamedKey::ArrowUp => b"\x1bOA",
        NamedKey::ArrowDown => b"\x1bOB",
        NamedKey::ArrowRight => b"\x1bOC",
        NamedKey::ArrowLeft => b"\x1bOD",
        NamedKey::Home => b"\x1bOH",
        NamedKey::End => b"\x1bOF",
        _ => return None,
    };
    Some(seq.to_vec())
}

/// Handle named-key + modifier combos that don't fit the arrow-key pattern.
pub fn modified_named_key(key: &NamedKey, modifiers: ModifiersState) -> Option<Vec<u8>> {
    match key {
        NamedKey::Tab if modifiers.shift_key() => Some(b"\x1b[Z".to_vec()),
        NamedKey::Space if modifiers.control_key() => Some(vec![0x00]),
        _ => None,
    }
}

/// Alt+Left/Right use readline word motion; Cmd+Left/Right use Home/End.
/// AppKit may consume Cmd+arrows before winit receives them on macOS.
pub fn modified_arrow(
    key: &NamedKey,
    modifiers: ModifiersState,
    cursor_app_mode: bool,
) -> Option<Vec<u8>> {
    let is_alt = modifiers.alt_key();
    let is_super = modifiers.super_key();

    if !is_alt && !is_super {
        return None;
    }

    let seq: &[u8] = match (key, is_alt, is_super) {
        (NamedKey::ArrowLeft, true, _) => b"\x1bb",
        (NamedKey::ArrowRight, true, _) => b"\x1bf",
        (NamedKey::ArrowLeft, _, true) if cursor_app_mode => b"\x1bOH",
        (NamedKey::ArrowRight, _, true) if cursor_app_mode => b"\x1bOF",
        (NamedKey::ArrowLeft, _, true) => b"\x1b[H",
        (NamedKey::ArrowRight, _, true) => b"\x1b[F",
        _ => return None,
    };

    Some(seq.to_vec())
}

#[cfg(test)]
mod tests {
    use winit::keyboard::{NamedKey, SmolStr};

    use super::*;

    #[test]
    fn space() {
        assert_eq!(named_key_bytes(&NamedKey::Space), Some(b" ".to_vec()));
    }

    #[test]
    fn enter() {
        assert_eq!(named_key_bytes(&NamedKey::Enter), Some(b"\r".to_vec()));
    }

    #[test]
    fn backspace() {
        assert_eq!(named_key_bytes(&NamedKey::Backspace), Some(vec![0x7f]));
    }

    #[test]
    fn tab() {
        assert_eq!(named_key_bytes(&NamedKey::Tab), Some(b"\t".to_vec()));
    }

    #[test]
    fn escape() {
        assert_eq!(named_key_bytes(&NamedKey::Escape), Some(vec![0x1b]));
    }

    #[test]
    fn arrow_up() {
        assert_eq!(named_key_bytes(&NamedKey::ArrowUp), Some(b"\x1b[A".to_vec()));
    }

    #[test]
    fn arrow_down() {
        assert_eq!(named_key_bytes(&NamedKey::ArrowDown), Some(b"\x1b[B".to_vec()));
    }

    #[test]
    fn arrow_right() {
        assert_eq!(named_key_bytes(&NamedKey::ArrowRight), Some(b"\x1b[C".to_vec()));
    }

    #[test]
    fn arrow_left() {
        assert_eq!(named_key_bytes(&NamedKey::ArrowLeft), Some(b"\x1b[D".to_vec()));
    }

    #[test]
    fn home() {
        assert_eq!(named_key_bytes(&NamedKey::Home), Some(b"\x1b[H".to_vec()));
    }

    #[test]
    fn end() {
        assert_eq!(named_key_bytes(&NamedKey::End), Some(b"\x1b[F".to_vec()));
    }

    #[test]
    fn page_up() {
        assert_eq!(named_key_bytes(&NamedKey::PageUp), Some(b"\x1b[5~".to_vec()));
    }

    #[test]
    fn page_down() {
        assert_eq!(named_key_bytes(&NamedKey::PageDown), Some(b"\x1b[6~".to_vec()));
    }

    #[test]
    fn delete() {
        assert_eq!(named_key_bytes(&NamedKey::Delete), Some(b"\x1b[3~".to_vec()));
    }

    #[test]
    fn insert() {
        assert_eq!(named_key_bytes(&NamedKey::Insert), Some(b"\x1b[2~".to_vec()));
    }

    #[test]
    fn f1() {
        assert_eq!(named_key_bytes(&NamedKey::F1), Some(b"\x1bOP".to_vec()));
    }

    #[test]
    fn f2() {
        assert_eq!(named_key_bytes(&NamedKey::F2), Some(b"\x1bOQ".to_vec()));
    }

    #[test]
    fn f3() {
        assert_eq!(named_key_bytes(&NamedKey::F3), Some(b"\x1bOR".to_vec()));
    }

    #[test]
    fn f4() {
        assert_eq!(named_key_bytes(&NamedKey::F4), Some(b"\x1bOS".to_vec()));
    }

    #[test]
    fn f5() {
        assert_eq!(named_key_bytes(&NamedKey::F5), Some(b"\x1b[15~".to_vec()));
    }

    #[test]
    fn f6() {
        assert_eq!(named_key_bytes(&NamedKey::F6), Some(b"\x1b[17~".to_vec()));
    }

    #[test]
    fn f7() {
        assert_eq!(named_key_bytes(&NamedKey::F7), Some(b"\x1b[18~".to_vec()));
    }

    #[test]
    fn f8() {
        assert_eq!(named_key_bytes(&NamedKey::F8), Some(b"\x1b[19~".to_vec()));
    }

    #[test]
    fn f9() {
        assert_eq!(named_key_bytes(&NamedKey::F9), Some(b"\x1b[20~".to_vec()));
    }

    #[test]
    fn f10() {
        assert_eq!(named_key_bytes(&NamedKey::F10), Some(b"\x1b[21~".to_vec()));
    }

    #[test]
    fn f11() {
        assert_eq!(named_key_bytes(&NamedKey::F11), Some(b"\x1b[23~".to_vec()));
    }

    #[test]
    fn f12() {
        assert_eq!(named_key_bytes(&NamedKey::F12), Some(b"\x1b[24~".to_vec()));
    }

    #[test]
    fn shift_returns_none() {
        assert_eq!(named_key_bytes(&NamedKey::Shift), None);
    }

    #[test]
    fn ctrl_returns_none() {
        assert_eq!(named_key_bytes(&NamedKey::Control), None);
    }

    #[test]
    fn alt_returns_none() {
        assert_eq!(named_key_bytes(&NamedKey::Alt), None);
    }

    #[test]
    fn super_returns_none() {
        assert_eq!(named_key_bytes(&NamedKey::Super), None);
    }

    #[test]
    fn ctrl_a() {
        assert_eq!(ctrl_char("a"), Some(0x01));
    }

    #[test]
    fn ctrl_c_synthesized() {
        assert_eq!(ctrl_char("c"), Some(0x03));
    }

    #[test]
    fn ctrl_d() {
        assert_eq!(ctrl_char("d"), Some(0x04));
    }

    #[test]
    fn ctrl_z() {
        assert_eq!(ctrl_char("z"), Some(0x1A));
    }

    #[test]
    fn ctrl_uppercase_c() {
        assert_eq!(ctrl_char("C"), Some(0x03));
    }

    #[test]
    fn ctrl_bracket() {
        assert_eq!(ctrl_char("["), Some(0x1B)); // ESC
    }

    #[test]
    fn ctrl_at() {
        assert_eq!(ctrl_char("@"), Some(0x00)); // NUL
    }

    #[test]
    fn ctrl_non_alpha_returns_none() {
        assert_eq!(ctrl_char("1"), None);
    }

    #[test]
    fn char_text_present() {
        let text = SmolStr::new("a");
        let bytes: Vec<u8> = text.as_bytes().to_vec();
        assert_eq!(bytes, b"a");
    }

    #[test]
    fn ctrl_c_via_text() {
        let text = SmolStr::new("\x03");
        assert_eq!(text.as_bytes(), &[0x03]);
    }

    #[test]
    fn utf8_multibyte() {
        let text = SmolStr::new("é");
        assert_eq!(text.as_bytes(), "é".as_bytes());
        assert_eq!(text.as_bytes(), &[0xC3, 0xA9]);
    }

    #[test]
    fn char_fallback_no_text() {
        let s = SmolStr::new("z");
        let bytes: Vec<u8> = s.as_bytes().to_vec();
        assert_eq!(bytes, b"z");
    }

    #[test]
    fn all_named_keys_do_not_panic() {
        let keys = [
            NamedKey::Space,
            NamedKey::Enter,
            NamedKey::Backspace,
            NamedKey::Tab,
            NamedKey::Escape,
            NamedKey::ArrowUp,
            NamedKey::ArrowDown,
            NamedKey::ArrowLeft,
            NamedKey::ArrowRight,
            NamedKey::Home,
            NamedKey::End,
            NamedKey::PageUp,
            NamedKey::PageDown,
            NamedKey::Delete,
            NamedKey::Insert,
            NamedKey::F1,
            NamedKey::F2,
            NamedKey::F3,
            NamedKey::F4,
            NamedKey::F5,
            NamedKey::F6,
            NamedKey::F7,
            NamedKey::F8,
            NamedKey::F9,
            NamedKey::F10,
            NamedKey::F11,
            NamedKey::F12,
            NamedKey::Shift,
            NamedKey::Control,
            NamedKey::Alt,
            NamedKey::Super,
            NamedKey::CapsLock,
            NamedKey::NumLock,
            NamedKey::ScrollLock,
            NamedKey::PrintScreen,
            NamedKey::Pause,
            NamedKey::ContextMenu,
        ];
        for key in &keys {
            let _ = named_key_bytes(key);
        }
    }

    #[test]
    fn ctrl_char_covers_full_alphabet() {
        for c in 'a'..='z' {
            let s = c.to_string();
            let byte = ctrl_char(&s).unwrap_or_else(|| panic!("ctrl_char should handle '{c}'"));
            assert_eq!(byte, c as u8 - b'a' + 1);
        }
    }

    #[test]
    fn ctrl_char_uppercase_matches_lowercase() {
        for (lower, upper) in ('a'..='z').zip('A'..='Z') {
            let lower_s = lower.to_string();
            let upper_s = upper.to_string();
            assert_eq!(ctrl_char(&lower_s), ctrl_char(&upper_s));
        }
    }

    #[test]
    fn opt_arrow_left_is_backward_word() {
        let mods = ModifiersState::ALT;
        assert_eq!(modified_arrow(&NamedKey::ArrowLeft, mods, false), Some(b"\x1bb".to_vec()));
    }

    #[test]
    fn opt_arrow_right_is_forward_word() {
        let mods = ModifiersState::ALT;
        assert_eq!(modified_arrow(&NamedKey::ArrowRight, mods, false), Some(b"\x1bf".to_vec()));
    }

    #[test]
    fn cmd_arrow_left_is_line_start() {
        let mods = ModifiersState::SUPER;
        assert_eq!(modified_arrow(&NamedKey::ArrowLeft, mods, false), Some(b"\x1b[H".to_vec()));
    }

    #[test]
    fn cmd_arrow_right_is_line_end() {
        let mods = ModifiersState::SUPER;
        assert_eq!(modified_arrow(&NamedKey::ArrowRight, mods, false), Some(b"\x1b[F".to_vec()));
    }

    #[test]
    fn unmodified_arrow_falls_through() {
        let mods = ModifiersState::empty();
        assert_eq!(modified_arrow(&NamedKey::ArrowLeft, mods, false), None);
        assert_eq!(modified_arrow(&NamedKey::ArrowRight, mods, false), None);
    }

    #[test]
    fn non_arrow_keys_ignored() {
        let mods = ModifiersState::ALT;
        assert_eq!(modified_arrow(&NamedKey::Enter, mods, false), None);
        assert_eq!(modified_arrow(&NamedKey::Backspace, mods, false), None);
    }

    #[test]
    fn opt_arrow_up_and_down_ignored() {
        let mods = ModifiersState::ALT;
        assert_eq!(modified_arrow(&NamedKey::ArrowUp, mods, false), None);
        assert_eq!(modified_arrow(&NamedKey::ArrowDown, mods, false), None);
    }

    #[test]
    fn shift_tab_is_reverse_tab() {
        let mods = ModifiersState::SHIFT;
        assert_eq!(modified_named_key(&NamedKey::Tab, mods), Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn ctrl_space_is_nul() {
        let mods = ModifiersState::CONTROL;
        assert_eq!(modified_named_key(&NamedKey::Space, mods), Some(vec![0x00]));
    }

    #[test]
    fn tab_without_modifier_unchanged() {
        assert_eq!(named_key_bytes(&NamedKey::Tab), Some(b"\t".to_vec()));
    }

    #[test]
    fn arrow_up_app_mode() {
        assert_eq!(named_key_bytes_app_mode(&NamedKey::ArrowUp), Some(b"\x1bOA".to_vec()));
    }

    #[test]
    fn arrow_down_app_mode() {
        assert_eq!(named_key_bytes_app_mode(&NamedKey::ArrowDown), Some(b"\x1bOB".to_vec()));
    }

    #[test]
    fn arrow_right_app_mode() {
        assert_eq!(named_key_bytes_app_mode(&NamedKey::ArrowRight), Some(b"\x1bOC".to_vec()));
    }

    #[test]
    fn arrow_left_app_mode() {
        assert_eq!(named_key_bytes_app_mode(&NamedKey::ArrowLeft), Some(b"\x1bOD".to_vec()));
    }

    #[test]
    fn home_app_mode() {
        assert_eq!(named_key_bytes_app_mode(&NamedKey::Home), Some(b"\x1bOH".to_vec()));
    }

    #[test]
    fn end_app_mode() {
        assert_eq!(named_key_bytes_app_mode(&NamedKey::End), Some(b"\x1bOF".to_vec()));
    }

    #[test]
    fn cmd_arrow_left_app_mode() {
        let mods = ModifiersState::SUPER;
        assert_eq!(modified_arrow(&NamedKey::ArrowLeft, mods, true), Some(b"\x1bOH".to_vec()));
    }

    #[test]
    fn cmd_arrow_right_app_mode() {
        let mods = ModifiersState::SUPER;
        assert_eq!(modified_arrow(&NamedKey::ArrowRight, mods, true), Some(b"\x1bOF".to_vec()));
    }

    #[test]
    fn arrow_up_non_app_mode() {
        assert_eq!(named_key_bytes(&NamedKey::ArrowUp), Some(b"\x1b[A".to_vec()));
    }
}
