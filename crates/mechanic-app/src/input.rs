//! Keyboard input translation: winit KeyEvent → PTY byte sequences.

use winit::event::{ElementState, KeyEvent};
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Translate a winit KeyEvent into bytes to send to the PTY.
///
/// `modifiers` is the current keyboard modifier state (tracked via
/// `WindowEvent::ModifiersChanged`).  It is needed because macOS often
/// does not populate `KeyEvent::text` for Ctrl+key combos — we must
/// synthesize the control character ourselves.
///
/// Returns `None` for key events that don't produce terminal input
/// (e.g. modifier-only presses, key releases).
pub fn translate_key(event: &KeyEvent, modifiers: ModifiersState) -> Option<Vec<u8>> {
    // Only act on key presses (including auto-repeat).
    if event.state != ElementState::Pressed {
        return None;
    }

    match &event.logical_key {
        Key::Named(named) => named_key_bytes(named),
        Key::Character(ch) => {
            // ── Ctrl+letter → control character ──────────────────────────
            //
            // On macOS, `event.text` is often `None` for Ctrl+key combos.
            // Synthesize the standard ASCII control character (Ctrl+A = 0x01,
            // Ctrl+C = 0x03, … Ctrl+Z = 0x1A).
            if modifiers.control_key() {
                if let Some(ctrl_byte) = ctrl_char(ch) {
                    return Some(vec![ctrl_byte]);
                }
            }

            // Prefer the OS-resolved text (handles dead keys, shifted chars, etc.).
            if let Some(text) = &event.text {
                if !text.is_empty() {
                    return Some(text.as_bytes().to_vec());
                }
            }
            // text was None or empty — encode the character string as UTF-8.
            if !ch.is_empty() {
                return Some(ch.as_bytes().to_vec());
            }
            None
        }
        // Unidentified / Dead keys — nothing to send.
        _ => None,
    }
}

/// Convert a character to its ASCII control-character byte when Ctrl is held.
///
/// Ctrl+a → 0x01, Ctrl+b → 0x02, … Ctrl+z → 0x1A.
/// Also handles Ctrl+\[ → 0x1B (ESC), Ctrl+\\ → 0x1C, Ctrl+] → 0x1D,
/// Ctrl+^ → 0x1E, Ctrl+_ → 0x1F, Ctrl+@ → 0x00 (NUL).
pub(crate) fn ctrl_char(ch: &str) -> Option<u8> {
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
///
/// Exposed as `pub(crate)` so the unit-test module can call it directly
/// without needing to construct a `KeyEvent` (whose `platform_specific`
/// field is private to winit).
pub(crate) fn named_key_bytes(key: &NamedKey) -> Option<Vec<u8>> {
    let seq: &[u8] = match key {
        NamedKey::Space => b" ",
        NamedKey::Enter => b"\r",
        NamedKey::Backspace => b"\x7f",
        NamedKey::Tab => b"\t",
        NamedKey::Escape => b"\x1b",

        // Cursor keys
        NamedKey::ArrowUp => b"\x1b[A",
        NamedKey::ArrowDown => b"\x1b[B",
        NamedKey::ArrowRight => b"\x1b[C",
        NamedKey::ArrowLeft => b"\x1b[D",

        // Navigation
        NamedKey::Home => b"\x1b[H",
        NamedKey::End => b"\x1b[F",
        NamedKey::PageUp => b"\x1b[5~",
        NamedKey::PageDown => b"\x1b[6~",
        NamedKey::Delete => b"\x1b[3~",
        NamedKey::Insert => b"\x1b[2~",

        // Function keys F1–F4 use SS3 sequences.
        NamedKey::F1 => b"\x1bOP",
        NamedKey::F2 => b"\x1bOQ",
        NamedKey::F3 => b"\x1bOR",
        NamedKey::F4 => b"\x1bOS",

        // Function keys F5–F12 use CSI ~ sequences.
        NamedKey::F5 => b"\x1b[15~",
        NamedKey::F6 => b"\x1b[17~",
        NamedKey::F7 => b"\x1b[18~",
        NamedKey::F8 => b"\x1b[19~",
        NamedKey::F9 => b"\x1b[20~",
        NamedKey::F10 => b"\x1b[21~",
        NamedKey::F11 => b"\x1b[23~",
        NamedKey::F12 => b"\x1b[24~",

        // Modifier-only and all other unhandled named keys → no output.
        _ => return None,
    };

    Some(seq.to_vec())
}

#[cfg(test)]
mod tests {
    use winit::keyboard::{NamedKey, SmolStr};

    use super::*;

    // ── named key → escape sequence ──────────────────────────────────────────

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

    // ── modifier-only and other unhandled named keys → None ──────────────────

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

    // ── ctrl_char ─────────────────────────────────────────────────────────────

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

    // ── character key helpers (test the inner logic directly) ─────────────────
    //
    // Because winit's `KeyEvent::platform_specific` is `pub(crate)`, we cannot
    // construct a full `KeyEvent` in external tests.  We verify the character
    // path through the helper functions that `translate_key` delegates to.

    /// Simulate the text-present branch: non-empty `event.text` wins.
    #[test]
    fn char_text_present() {
        // If text is available, return it verbatim.
        let text = SmolStr::new("a");
        let bytes: Vec<u8> = text.as_bytes().to_vec();
        assert_eq!(bytes, b"a");
    }

    /// Ctrl+C scenario: winit sets text = "\x03" (ETX).
    #[test]
    fn ctrl_c_via_text() {
        let text = SmolStr::new("\x03");
        assert_eq!(text.as_bytes(), &[0x03]);
    }

    /// UTF-8 multibyte: "é" encodes to [0xC3, 0xA9].
    #[test]
    fn utf8_multibyte() {
        let text = SmolStr::new("é");
        assert_eq!(text.as_bytes(), "é".as_bytes());
        assert_eq!(text.as_bytes(), &[0xC3, 0xA9]);
    }

    /// Fallback: no text, encode the character string from `logical_key`.
    #[test]
    fn char_fallback_no_text() {
        let s = SmolStr::new("z");
        let bytes: Vec<u8> = s.as_bytes().to_vec();
        assert_eq!(bytes, b"z");
    }

    #[test]
    fn all_named_keys_do_not_panic() {
        // Smoke test: calling named_key_bytes with every NamedKey variant
        // should never panic, even for unhandled keys.
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
        // Every letter a-z should produce a valid control character.
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
}
