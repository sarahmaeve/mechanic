//! Kitty keyboard encoding, following https://sw.kovidgoyal.net/kitty/keyboard-protocol/.

use alacritty_terminal::vte::ansi::KeyboardModes;
use winit::event::ElementState;
use winit::keyboard::{Key, KeyCode, KeyLocation, ModifiersState, NamedKey, PhysicalKey};

/// Separate the public winit event from its private platform data for testing.
#[derive(Clone, Copy)]
pub(crate) struct KeyInput<'a> {
    pub state: ElementState,
    pub logical_key: &'a Key,
    pub unmodified_key: &'a Key,
    pub physical_key: PhysicalKey,
    pub location: KeyLocation,
    pub text: Option<&'a str>,
    pub repeat: bool,
}

pub(super) fn encodes_keys(modes: KeyboardModes) -> bool {
    modes.intersects(
        KeyboardModes::DISAMBIGUATE_ESC_CODES
            | KeyboardModes::REPORT_EVENT_TYPES
            | KeyboardModes::REPORT_ALL_KEYS_AS_ESC,
    )
}

pub(super) fn translate(
    input: KeyInput<'_>,
    modifiers: ModifiersState,
    modes: KeyboardModes,
) -> Option<Vec<u8>> {
    translate_for_platform(input, modifiers, modes, cfg!(target_os = "macos"))
}

fn translate_for_platform(
    input: KeyInput<'_>,
    modifiers: ModifiersState,
    modes: KeyboardModes,
    macos: bool,
) -> Option<Vec<u8>> {
    let all = modes.contains(KeyboardModes::REPORT_ALL_KEYS_AS_ESC);
    let disambiguate = modes.contains(KeyboardModes::DISAMBIGUATE_ESC_CODES);
    let events = modes.contains(KeyboardModes::REPORT_EVENT_TYPES);
    let release = input.state == ElementState::Released;

    if release && !events {
        return None;
    }
    if matches!(input.logical_key, Key::Dead(_)) {
        // Composition text is delivered separately, through IME commits.
        return None;
    }

    let named = match input.logical_key {
        Key::Named(named) => Some(*named),
        _ => None,
    };
    if named.is_some_and(is_modifier) && !all {
        return None;
    }

    // Keep these keys usable at a shell after a crashed application. Modified
    // presses are disambiguated, but releases require report-all, per the spec.
    let shell_key = matches!(named, Some(NamedKey::Enter | NamedKey::Tab | NamedKey::Backspace))
        && !(disambiguate && input.location == KeyLocation::Numpad);
    if shell_key && !all {
        if release {
            return None;
        }
        if !disambiguate || modifiers.is_empty() {
            return super::translate_input(
                input.state,
                input.logical_key,
                input.text,
                modifiers,
                false,
            );
        }
    }

    let text = input.text.filter(|text| printable_text(text));
    let option_composed_key = macos
        && modifiers.alt_key()
        && !modifiers.control_key()
        && !modifiers.super_key()
        && input.logical_key != input.unmodified_key
        && matches!(input.logical_key, Key::Character(_));
    let option_composed_text = option_composed_key
        && matches!(input.logical_key, Key::Character(logical) if text == Some(logical.as_str()));
    if option_composed_text
        && !all
        && !release
        && let Some(text) = text
    {
        // macOS consumes Option to compose layout text. The unmodified key
        // distinguishes this from an unchanged Alt shortcut. Kitty likewise
        // sends generated text directly when report-all was not requested.
        return Some(text.as_bytes().to_vec());
    }
    if !all
        && !disambiguate
        && !release
        && (!input.repeat || text.is_some())
        && !modifiers.super_key()
        && !(modifiers.control_key() && modifiers.shift_key())
        && matches!(input.logical_key, Key::Character(_) | Key::Named(NamedKey::Space))
    {
        // Event reporting alone does not disambiguate ordinary legacy presses.
        // Non-text repeats/releases carry the requested action sub-field.
        return super::translate_input(
            input.state,
            input.logical_key,
            input.text,
            modifiers,
            false,
        );
    }
    let shortcut =
        modifiers.intersects(ModifiersState::CONTROL | ModifiersState::ALT | ModifiersState::SUPER);
    let keypad = input.location == KeyLocation::Numpad;
    let escape = (release && events)
        || all
        || (disambiguate && (shortcut || keypad || named == Some(NamedKey::Escape)))
        || (events && shortcut)
        || named.is_some_and(|key| key != NamedKey::Space);

    if !escape {
        // Text presses/repeats stay UTF-8 without report-all. Releases have no
        // text and use CSI under report-events, as in Kitty's key_encoding.c
        // and Alacritty, despite the specification's terse note on text events.
        if release {
            return None;
        }
        return super::translate_input(
            input.state,
            input.logical_key,
            input.text,
            modifiers,
            false,
        );
    }

    let (code, terminator) = if keypad && (disambiguate || all) {
        keypad_code(input).or_else(|| named.and_then(|key| named_code(key, input.location)))
    } else {
        named.and_then(|key| named_code(key, input.location))
    }
    .or_else(|| {
        (keypad && !disambiguate && !all)
            .then(|| character(input.logical_key).map(|key| (u32::from(key), b'u')))
            .flatten()
    })
    .or_else(|| character(input.unmodified_key).map(|key| (u32::from(key), b'u')))
    .or_else(|| {
        // Never take the first scalar of a composed/multi-scalar key as its
        // identity. Associated text can represent it as a pure text event.
        (all && modes.contains(KeyboardModes::REPORT_ASSOCIATED_TEXT) && text.is_some())
            .then_some((0, b'u'))
    })?;

    let alternate = modes.contains(KeyboardModes::REPORT_ALTERNATE_KEYS)
        && terminator == b'u'
        && character(input.unmodified_key).is_some_and(|key| u32::from(key) == code);
    let shifted = alternate
        .then(|| character(input.logical_key))
        .flatten()
        // Option-composed logical text is not the Shift-only alternate; winit
        // does not expose that alternate, so omit it rather than report text
        // composition as the shifted key (including on releases without text).
        .filter(|key| modifiers.shift_key() && !option_composed_key && u32::from(*key) != code);
    let base = alternate
        .then(|| base_layout_key(input.physical_key))
        .flatten()
        .filter(|key| u32::from(*key) != code && character(input.unmodified_key).is_some());
    let associated = (all && modes.contains(KeyboardModes::REPORT_ASSOCIATED_TEXT) && !release)
        .then_some(text)
        .flatten();
    let event_type =
        if events && (release || input.repeat) { Some(if release { 3 } else { 2 }) } else { None };

    Some(encode(code, terminator, modifiers, shifted, base, event_type, associated))
}

pub(super) fn translate_text(text: &str, modes: KeyboardModes) -> Option<Vec<u8>> {
    if text.is_empty() {
        return None;
    }
    if !modes
        .contains(KeyboardModes::REPORT_ALL_KEYS_AS_ESC | KeyboardModes::REPORT_ASSOCIATED_TEXT)
    {
        // IME commits have no corresponding key identity. Preserve their UTF-8
        // when embedded text was not requested, as Kitty's IME commit path does.
        return Some(text.as_bytes().to_vec());
    }
    if !printable_text(text) {
        return None;
    }
    Some(encode(0, b'u', ModifiersState::empty(), None, None, None, Some(text)))
}

fn printable_text(text: &str) -> bool {
    !text.is_empty() && !text.chars().any(char::is_control)
}

fn character(key: &Key) -> Option<char> {
    match key {
        Key::Character(text) => {
            let mut chars = text.chars();
            let ch = chars.next()?;
            (chars.next().is_none() && !ch.is_control()).then_some(ch)
        }
        Key::Named(NamedKey::Space) => Some(' '),
        _ => None,
    }
}

fn is_modifier(key: NamedKey) -> bool {
    matches!(
        key,
        NamedKey::Shift
            | NamedKey::Control
            | NamedKey::Alt
            | NamedKey::Super
            | NamedKey::Hyper
            | NamedKey::Meta
            | NamedKey::AltGraph
    )
}

fn encode(
    code: u32,
    terminator: u8,
    modifiers: ModifiersState,
    shifted: Option<char>,
    base: Option<char>,
    event_type: Option<u32>,
    text: Option<&str>,
) -> Vec<u8> {
    let mods = u32::from(modifiers.shift_key())
        | (u32::from(modifiers.alt_key()) << 1)
        | (u32::from(modifiers.control_key()) << 2)
        | (u32::from(modifiers.super_key()) << 3);
    let parameters = mods != 0 || event_type.is_some() || text.is_some();
    let mut bytes = Vec::with_capacity(32 + text.map_or(0, str::len) * 4);
    bytes.extend_from_slice(b"\x1b[");
    // CSI's letter forms have a default first parameter of 1.
    if terminator == b'u' || terminator == b'~' || parameters {
        decimal(&mut bytes, code);
    }
    if shifted.is_some() || base.is_some() {
        bytes.push(b':');
        if let Some(shifted) = shifted {
            decimal(&mut bytes, shifted as u32);
        }
        if let Some(base) = base {
            bytes.push(b':');
            decimal(&mut bytes, base as u32);
        }
    }
    if parameters {
        bytes.push(b';');
        decimal(&mut bytes, mods + 1);
    }
    if let Some(event_type) = event_type {
        bytes.push(b':');
        decimal(&mut bytes, event_type);
    }
    if let Some(text) = text {
        bytes.push(b';');
        for (index, ch) in text.chars().enumerate() {
            if index != 0 {
                bytes.push(b':');
            }
            decimal(&mut bytes, ch as u32);
        }
    }
    bytes.push(terminator);
    bytes
}

fn decimal(bytes: &mut Vec<u8>, mut number: u32) {
    let mut digits = [0; 10];
    let mut index = digits.len();
    loop {
        index -= 1;
        digits[index] = b'0' + (number % 10) as u8;
        number /= 10;
        if number == 0 {
            break;
        }
    }
    bytes.extend_from_slice(&digits[index..]);
}

fn named_code(key: NamedKey, location: KeyLocation) -> Option<(u32, u8)> {
    let right = location == KeyLocation::Right;
    let code = match key {
        NamedKey::Escape => 27,
        NamedKey::Enter => 13,
        NamedKey::Tab => 9,
        NamedKey::Backspace => 127,
        NamedKey::Space => 32,
        NamedKey::Insert => return Some((2, b'~')),
        NamedKey::Delete => return Some((3, b'~')),
        NamedKey::ArrowLeft => return Some((1, b'D')),
        NamedKey::ArrowRight => return Some((1, b'C')),
        NamedKey::ArrowUp => return Some((1, b'A')),
        NamedKey::ArrowDown => return Some((1, b'B')),
        NamedKey::PageUp => return Some((5, b'~')),
        NamedKey::PageDown => return Some((6, b'~')),
        NamedKey::Home => return Some((1, b'H')),
        NamedKey::End => return Some((1, b'F')),
        NamedKey::CapsLock => 57358,
        NamedKey::ScrollLock => 57359,
        NamedKey::NumLock => 57360,
        NamedKey::PrintScreen => 57361,
        NamedKey::Pause => 57362,
        NamedKey::ContextMenu => 57363,
        NamedKey::F1 => return Some((1, b'P')),
        NamedKey::F2 => return Some((1, b'Q')),
        // CSI R conflicts with cursor-position reports.
        NamedKey::F3 => return Some((13, b'~')),
        NamedKey::F4 => return Some((1, b'S')),
        NamedKey::F5 => return Some((15, b'~')),
        NamedKey::F6 => return Some((17, b'~')),
        NamedKey::F7 => return Some((18, b'~')),
        NamedKey::F8 => return Some((19, b'~')),
        NamedKey::F9 => return Some((20, b'~')),
        NamedKey::F10 => return Some((21, b'~')),
        NamedKey::F11 => return Some((23, b'~')),
        NamedKey::F12 => return Some((24, b'~')),
        NamedKey::F13 => 57376,
        NamedKey::F14 => 57377,
        NamedKey::F15 => 57378,
        NamedKey::F16 => 57379,
        NamedKey::F17 => 57380,
        NamedKey::F18 => 57381,
        NamedKey::F19 => 57382,
        NamedKey::F20 => 57383,
        NamedKey::F21 => 57384,
        NamedKey::F22 => 57385,
        NamedKey::F23 => 57386,
        NamedKey::F24 => 57387,
        NamedKey::F25 => 57388,
        NamedKey::F26 => 57389,
        NamedKey::F27 => 57390,
        NamedKey::F28 => 57391,
        NamedKey::F29 => 57392,
        NamedKey::F30 => 57393,
        NamedKey::F31 => 57394,
        NamedKey::F32 => 57395,
        NamedKey::F33 => 57396,
        NamedKey::F34 => 57397,
        NamedKey::F35 => 57398,
        NamedKey::MediaPlay => 57428,
        NamedKey::MediaPause => 57429,
        NamedKey::MediaPlayPause => 57430,
        NamedKey::MediaStop => 57432,
        NamedKey::MediaFastForward => 57433,
        NamedKey::MediaRewind => 57434,
        NamedKey::MediaTrackNext => 57435,
        NamedKey::MediaTrackPrevious => 57436,
        NamedKey::MediaRecord => 57437,
        NamedKey::AudioVolumeDown => 57438,
        NamedKey::AudioVolumeUp => 57439,
        NamedKey::AudioVolumeMute => 57440,
        NamedKey::Shift => {
            if right {
                57447
            } else {
                57441
            }
        }
        NamedKey::Control => {
            if right {
                57448
            } else {
                57442
            }
        }
        NamedKey::Alt => {
            if right {
                57449
            } else {
                57443
            }
        }
        NamedKey::Super => {
            if right {
                57450
            } else {
                57444
            }
        }
        NamedKey::Hyper => {
            if right {
                57451
            } else {
                57445
            }
        }
        NamedKey::Meta => {
            if right {
                57452
            } else {
                57446
            }
        }
        NamedKey::AltGraph => 57453,
        _ => return None,
    };
    Some((code, b'u'))
}

fn keypad_code(input: KeyInput<'_>) -> Option<(u32, u8)> {
    // Logical navigation identity must win over the physical digit when Num
    // Lock is off. Physical identity handles locale-specific decimal separators.
    let code = match input.logical_key {
        Key::Named(NamedKey::Enter) => 57414,
        Key::Named(NamedKey::ArrowLeft) => 57417,
        Key::Named(NamedKey::ArrowRight) => 57418,
        Key::Named(NamedKey::ArrowUp) => 57419,
        Key::Named(NamedKey::ArrowDown) => 57420,
        Key::Named(NamedKey::PageUp) => 57421,
        Key::Named(NamedKey::PageDown) => 57422,
        Key::Named(NamedKey::Home) => 57423,
        Key::Named(NamedKey::End) => 57424,
        Key::Named(NamedKey::Insert) => 57425,
        Key::Named(NamedKey::Delete) => 57426,
        Key::Named(NamedKey::Clear) => return Some((1, b'E')),
        _ => match input.physical_key {
            PhysicalKey::Code(KeyCode::Numpad0) => 57399,
            PhysicalKey::Code(KeyCode::Numpad1) => 57400,
            PhysicalKey::Code(KeyCode::Numpad2) => 57401,
            PhysicalKey::Code(KeyCode::Numpad3) => 57402,
            PhysicalKey::Code(KeyCode::Numpad4) => 57403,
            PhysicalKey::Code(KeyCode::Numpad5) => 57404,
            PhysicalKey::Code(KeyCode::Numpad6) => 57405,
            PhysicalKey::Code(KeyCode::Numpad7) => 57406,
            PhysicalKey::Code(KeyCode::Numpad8) => 57407,
            PhysicalKey::Code(KeyCode::Numpad9) => 57408,
            PhysicalKey::Code(KeyCode::NumpadDecimal) => 57409,
            PhysicalKey::Code(KeyCode::NumpadDivide) => 57410,
            PhysicalKey::Code(KeyCode::NumpadMultiply) => 57411,
            PhysicalKey::Code(KeyCode::NumpadSubtract) => 57412,
            PhysicalKey::Code(KeyCode::NumpadAdd) => 57413,
            PhysicalKey::Code(KeyCode::NumpadEnter) => 57414,
            PhysicalKey::Code(KeyCode::NumpadEqual) => 57415,
            PhysicalKey::Code(KeyCode::NumpadComma) => 57416,
            _ => return None,
        },
    };
    Some((code, b'u'))
}

fn base_layout_key(physical: PhysicalKey) -> Option<char> {
    let PhysicalKey::Code(code) = physical else { return None };
    Some(match code {
        KeyCode::KeyA => 'a',
        KeyCode::KeyB => 'b',
        KeyCode::KeyC => 'c',
        KeyCode::KeyD => 'd',
        KeyCode::KeyE => 'e',
        KeyCode::KeyF => 'f',
        KeyCode::KeyG => 'g',
        KeyCode::KeyH => 'h',
        KeyCode::KeyI => 'i',
        KeyCode::KeyJ => 'j',
        KeyCode::KeyK => 'k',
        KeyCode::KeyL => 'l',
        KeyCode::KeyM => 'm',
        KeyCode::KeyN => 'n',
        KeyCode::KeyO => 'o',
        KeyCode::KeyP => 'p',
        KeyCode::KeyQ => 'q',
        KeyCode::KeyR => 'r',
        KeyCode::KeyS => 's',
        KeyCode::KeyT => 't',
        KeyCode::KeyU => 'u',
        KeyCode::KeyV => 'v',
        KeyCode::KeyW => 'w',
        KeyCode::KeyX => 'x',
        KeyCode::KeyY => 'y',
        KeyCode::KeyZ => 'z',
        KeyCode::Digit0 => '0',
        KeyCode::Digit1 => '1',
        KeyCode::Digit2 => '2',
        KeyCode::Digit3 => '3',
        KeyCode::Digit4 => '4',
        KeyCode::Digit5 => '5',
        KeyCode::Digit6 => '6',
        KeyCode::Digit7 => '7',
        KeyCode::Digit8 => '8',
        KeyCode::Digit9 => '9',
        KeyCode::Backquote => '`',
        KeyCode::Minus => '-',
        KeyCode::Equal => '=',
        KeyCode::BracketLeft => '[',
        KeyCode::BracketRight => ']',
        KeyCode::Backslash => '\\',
        KeyCode::Semicolon => ';',
        KeyCode::Quote => '\'',
        KeyCode::Comma => ',',
        KeyCode::Period => '.',
        KeyCode::Slash => '/',
        KeyCode::Space => ' ',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::hint::black_box;
    use std::time::Instant;

    use super::*;
    use crate::input::translate_input_with_modes;

    fn key<'a>(logical: &'a Key, unmodified: &'a Key, text: Option<&'a str>) -> KeyInput<'a> {
        KeyInput {
            state: ElementState::Pressed,
            logical_key: logical,
            unmodified_key: unmodified,
            physical_key: PhysicalKey::Code(KeyCode::KeyA),
            location: KeyLocation::Standard,
            text,
            repeat: false,
        }
    }

    fn check(input: KeyInput<'_>, mods: ModifiersState, flags: KeyboardModes, expected: &[u8]) {
        assert_eq!(translate_input_with_modes(input, mods, true, flags), Some(expected.to_vec()));
    }

    #[test]
    fn empty_flags_keep_legacy_sequences_modifiers_and_releases() {
        let named = Key::Named(NamedKey::ArrowLeft);
        let input = key(&named, &named, None);
        check(input, ModifiersState::empty(), KeyboardModes::empty(), b"\x1bOD");
        check(input, ModifiersState::ALT, KeyboardModes::empty(), b"\x1bb");
        check(input, ModifiersState::SUPER, KeyboardModes::empty(), b"\x1bOH");
        for flags in [
            KeyboardModes::empty(),
            KeyboardModes::REPORT_ALTERNATE_KEYS,
            KeyboardModes::REPORT_ASSOCIATED_TEXT,
            KeyboardModes::REPORT_ALTERNATE_KEYS | KeyboardModes::REPORT_ASSOCIATED_TEXT,
        ] {
            let ch = Key::Character("ü".into());
            let input = key(&ch, &ch, Some("ü"));
            check(input, ModifiersState::ALT, flags, "ü".as_bytes());
            assert_eq!(
                translate_input_with_modes(
                    KeyInput { state: ElementState::Released, ..input },
                    ModifiersState::ALT,
                    false,
                    flags,
                ),
                None
            );
        }
    }

    #[test]
    fn disambiguates_enter_shift_enter_ctrl_i_and_tab() {
        let flags = KeyboardModes::DISAMBIGUATE_ESC_CODES;
        let enter = Key::Named(NamedKey::Enter);
        let tab = Key::Named(NamedKey::Tab);
        let i = Key::Character("i".into());
        check(key(&enter, &enter, Some("\r")), ModifiersState::empty(), flags, b"\r");
        check(key(&enter, &enter, Some("\r")), ModifiersState::SHIFT, flags, b"\x1b[13;2u");
        check(key(&i, &i, Some("\t")), ModifiersState::CONTROL, flags, b"\x1b[105;5u");
        check(key(&tab, &tab, Some("\t")), ModifiersState::empty(), flags, b"\t");
        check(key(&tab, &tab, Some("\t")), ModifiersState::SHIFT, flags, b"\x1b[9;2u");
        check(
            key(&tab, &tab, None),
            ModifiersState::SHIFT | ModifiersState::CONTROL,
            flags,
            b"\x1b[9;6u",
        );
        let escape = Key::Named(NamedKey::Escape);
        check(key(&escape, &escape, Some("\x1b")), ModifiersState::empty(), flags, b"\x1b[27u");
    }

    #[test]
    fn all_modifier_combinations_have_protocol_bit_order() {
        let ch = Key::Character("a".into());
        for bits in 0..16u32 {
            let mut mods = ModifiersState::empty();
            mods.set(ModifiersState::SHIFT, bits & 1 != 0);
            mods.set(ModifiersState::ALT, bits & 2 != 0);
            mods.set(ModifiersState::CONTROL, bits & 4 != 0);
            mods.set(ModifiersState::SUPER, bits & 8 != 0);
            let expected =
                if bits == 0 { "\x1b[97u".to_owned() } else { format!("\x1b[97;{}u", bits + 1) };
            check(
                key(&ch, &ch, None),
                mods,
                KeyboardModes::REPORT_ALL_KEYS_AS_ESC,
                expected.as_bytes(),
            );
        }
    }

    #[test]
    fn every_flag_combination_respects_text_and_event_requests() {
        let ch = Key::Character("a".into());
        let input = key(&ch, &ch, Some("a"));
        for bits in 0..32 {
            let flags = KeyboardModes::from_bits_retain(bits);
            let all = bits & 8 != 0;
            let text = bits & 16 != 0;
            let events = bits & 2 != 0;
            let expected: &[u8] = if all && text {
                b"\x1b[97;1;97u"
            } else if all {
                b"\x1b[97u"
            } else {
                b"a"
            };
            check(input, ModifiersState::empty(), flags, expected);
            let repeated = KeyInput { repeat: true, ..input };
            let expected: &[u8] = if all && events && text {
                b"\x1b[97;1:2;97u"
            } else if all && events {
                b"\x1b[97;1:2u"
            } else if all && text {
                b"\x1b[97;1;97u"
            } else if all {
                b"\x1b[97u"
            } else {
                b"a"
            };
            check(repeated, ModifiersState::empty(), flags, expected);
            let released = KeyInput { state: ElementState::Released, ..input };
            let expected = events.then(|| b"\x1b[97;1:3u".to_vec());
            assert_eq!(
                translate_input_with_modes(released, ModifiersState::empty(), false, flags),
                expected
            );
        }
    }

    #[test]
    fn repeats_and_releases_escaped_keys_only_when_requested() {
        let left = Key::Named(NamedKey::ArrowLeft);
        let input = key(&left, &left, None);
        check(input, ModifiersState::CONTROL, KeyboardModes::REPORT_EVENT_TYPES, b"\x1b[1;5D");
        check(
            KeyInput { repeat: true, ..input },
            ModifiersState::CONTROL,
            KeyboardModes::REPORT_EVENT_TYPES,
            b"\x1b[1;5:2D",
        );
        check(
            KeyInput { state: ElementState::Released, ..input },
            ModifiersState::CONTROL,
            KeyboardModes::REPORT_EVENT_TYPES,
            b"\x1b[1;5:3D",
        );
        check(
            KeyInput { repeat: true, ..input },
            ModifiersState::CONTROL,
            KeyboardModes::DISAMBIGUATE_ESC_CODES,
            b"\x1b[1;5D",
        );
        assert_eq!(
            translate(
                KeyInput { state: ElementState::Released, ..input },
                ModifiersState::CONTROL,
                KeyboardModes::DISAMBIGUATE_ESC_CODES
            ),
            None
        );
    }

    #[test]
    fn event_reporting_pairs_literal_unicode_press_with_encoded_release() {
        let logical = Key::Character("Ü".into());
        let unmodified = Key::Character("ü".into());
        let input = key(&logical, &unmodified, Some("Ü"));
        let flags = KeyboardModes::REPORT_EVENT_TYPES;
        check(input, ModifiersState::SHIFT, flags, "Ü".as_bytes());
        check(KeyInput { repeat: true, ..input }, ModifiersState::SHIFT, flags, "Ü".as_bytes());
        check(
            KeyInput { state: ElementState::Released, text: None, ..input },
            ModifiersState::SHIFT,
            flags,
            b"\x1b[252;2:3u",
        );
    }

    #[test]
    fn event_reporting_alone_keeps_legacy_control_press_then_reports_actions() {
        let logical = Key::Character("i".into());
        let input = key(&logical, &logical, Some("\t"));
        let flags = KeyboardModes::REPORT_EVENT_TYPES;
        check(input, ModifiersState::CONTROL, flags, b"\t");
        check(KeyInput { repeat: true, ..input }, ModifiersState::CONTROL, flags, b"\x1b[105;5:2u");
        check(
            KeyInput { state: ElementState::Released, text: None, ..input },
            ModifiersState::CONTROL,
            flags,
            b"\x1b[105;5:3u",
        );
    }

    #[test]
    fn shell_controls_need_report_all_to_send_releases() {
        for named in [NamedKey::Enter, NamedKey::Tab, NamedKey::Backspace] {
            let logical = Key::Named(named);
            let input = KeyInput { state: ElementState::Released, ..key(&logical, &logical, None) };
            for mods in [ModifiersState::empty(), ModifiersState::SHIFT, ModifiersState::CONTROL] {
                assert_eq!(
                    translate(
                        input,
                        mods,
                        KeyboardModes::DISAMBIGUATE_ESC_CODES | KeyboardModes::REPORT_EVENT_TYPES
                    ),
                    None
                );
            }
        }
        let enter = Key::Named(NamedKey::Enter);
        check(
            KeyInput { state: ElementState::Released, ..key(&enter, &enter, Some("\r")) },
            ModifiersState::SHIFT,
            KeyboardModes::all(),
            b"\x1b[13;2:3u",
        );
    }

    #[test]
    fn functional_keys_use_canonical_forms_even_in_application_cursor_mode() {
        for (named, press, modified, release) in [
            (NamedKey::ArrowUp, "\x1b[A", "\x1b[1;3A", "\x1b[1;1:3A"),
            (NamedKey::ArrowDown, "\x1b[B", "\x1b[1;3B", "\x1b[1;1:3B"),
            (NamedKey::ArrowLeft, "\x1b[D", "\x1b[1;3D", "\x1b[1;1:3D"),
            (NamedKey::ArrowRight, "\x1b[C", "\x1b[1;3C", "\x1b[1;1:3C"),
            (NamedKey::Home, "\x1b[H", "\x1b[1;3H", "\x1b[1;1:3H"),
            (NamedKey::End, "\x1b[F", "\x1b[1;3F", "\x1b[1;1:3F"),
            (NamedKey::Insert, "\x1b[2~", "\x1b[2;3~", "\x1b[2;1:3~"),
            (NamedKey::Delete, "\x1b[3~", "\x1b[3;3~", "\x1b[3;1:3~"),
            (NamedKey::F1, "\x1b[P", "\x1b[1;3P", "\x1b[1;1:3P"),
            (NamedKey::F2, "\x1b[Q", "\x1b[1;3Q", "\x1b[1;1:3Q"),
            (NamedKey::F3, "\x1b[13~", "\x1b[13;3~", "\x1b[13;1:3~"),
            (NamedKey::F4, "\x1b[S", "\x1b[1;3S", "\x1b[1;1:3S"),
            (NamedKey::F12, "\x1b[24~", "\x1b[24;3~", "\x1b[24;1:3~"),
            (NamedKey::F35, "\x1b[57398u", "\x1b[57398;3u", "\x1b[57398;1:3u"),
        ] {
            let key = Key::Named(named);
            let input = key_input(&key);
            check(input, ModifiersState::empty(), KeyboardModes::all(), press.as_bytes());
            check(input, ModifiersState::ALT, KeyboardModes::all(), modified.as_bytes());
            check(
                KeyInput { state: ElementState::Released, ..input },
                ModifiersState::empty(),
                KeyboardModes::all(),
                release.as_bytes(),
            );
        }
    }

    fn key_input(key_value: &Key) -> KeyInput<'_> {
        key(key_value, key_value, None)
    }

    #[test]
    fn shifted_punctuation_uses_layout_unshifted_key_and_optional_alternate() {
        let shifted = Key::Character("+".into());
        let unshifted = Key::Character("=".into());
        let input = KeyInput {
            physical_key: PhysicalKey::Code(KeyCode::Equal),
            ..key(&shifted, &unshifted, Some("+"))
        };
        check(
            input,
            ModifiersState::SHIFT | ModifiersState::CONTROL,
            KeyboardModes::DISAMBIGUATE_ESC_CODES,
            b"\x1b[61;6u",
        );
        check(
            input,
            ModifiersState::SHIFT | ModifiersState::CONTROL,
            KeyboardModes::DISAMBIGUATE_ESC_CODES | KeyboardModes::REPORT_ALTERNATE_KEYS,
            b"\x1b[61:43;6u",
        );
        let shifted = Key::Character("\"".into());
        let unshifted = Key::Character("2".into());
        let input = KeyInput {
            physical_key: PhysicalKey::Code(KeyCode::Digit2),
            ..key(&shifted, &unshifted, Some("\""))
        };
        check(input, ModifiersState::SHIFT, KeyboardModes::all(), b"\x1b[50:34;2;34u");
    }

    #[test]
    fn macos_option_composition_keeps_text_and_unchanged_alt_shortcuts_escape() {
        for (composed, base, physical, encoded) in [
            ("å", "a", KeyCode::KeyA, "\x1b[97;3;229u"),
            ("œ", "q", KeyCode::KeyQ, "\x1b[113;3;339u"),
            ("{", "8", KeyCode::Digit8, "\x1b[56;3;123u"),
        ] {
            let logical = Key::Character(composed.into());
            let unmodified = Key::Character(base.into());
            let input = KeyInput {
                physical_key: PhysicalKey::Code(physical),
                ..key(&logical, &unmodified, Some(composed))
            };
            for flags in [
                KeyboardModes::DISAMBIGUATE_ESC_CODES,
                KeyboardModes::DISAMBIGUATE_ESC_CODES | KeyboardModes::REPORT_EVENT_TYPES,
            ] {
                assert_eq!(
                    translate_for_platform(input, ModifiersState::ALT, flags, true),
                    Some(composed.as_bytes().to_vec())
                );
                assert_eq!(
                    translate_for_platform(
                        KeyInput { repeat: true, ..input },
                        ModifiersState::ALT,
                        flags,
                        true
                    ),
                    Some(composed.as_bytes().to_vec())
                );
            }
            assert_eq!(
                translate_for_platform(input, ModifiersState::ALT, KeyboardModes::all(), true),
                Some(encoded.as_bytes().to_vec())
            );
            // Other platforms still treat Alt as a protocol shortcut modifier.
            assert_ne!(
                translate_for_platform(
                    input,
                    ModifiersState::ALT,
                    KeyboardModes::DISAMBIGUATE_ESC_CODES,
                    false
                ),
                Some(composed.as_bytes().to_vec())
            );
        }
        let a = Key::Character("a".into());
        assert_eq!(
            translate_for_platform(
                key(&a, &a, Some("a")),
                ModifiersState::ALT,
                KeyboardModes::DISAMBIGUATE_ESC_CODES,
                true
            ),
            Some(b"\x1b[97;3u".to_vec())
        );
        let dead = Key::Dead(Some('´'));
        let base = Key::Character("e".into());
        assert_eq!(
            translate_for_platform(
                key(&dead, &base, None),
                ModifiersState::ALT,
                KeyboardModes::all(),
                true
            ),
            None
        );
        assert_eq!(
            translate_text("é", KeyboardModes::DISAMBIGUATE_ESC_CODES),
            Some("é".as_bytes().to_vec())
        );
        let shifted = Key::Character("Å".into());
        let base = Key::Character("a".into());
        assert_eq!(
            translate_for_platform(
                key(&shifted, &base, Some("Å")),
                ModifiersState::ALT | ModifiersState::SHIFT,
                KeyboardModes::all(),
                true
            ),
            Some(b"\x1b[97;4;197u".to_vec())
        );
        assert_eq!(
            translate_for_platform(
                KeyInput { state: ElementState::Released, ..key(&shifted, &base, None) },
                ModifiersState::ALT | ModifiersState::SHIFT,
                KeyboardModes::all(),
                true
            ),
            Some(b"\x1b[97;4:3u".to_vec())
        );
    }

    #[test]
    fn unicode_layout_and_pc101_base_are_independent() {
        let cyrillic = Key::Character("с".into());
        let input = KeyInput {
            physical_key: PhysicalKey::Code(KeyCode::KeyC),
            ..key(&cyrillic, &cyrillic, None)
        };
        check(
            input,
            ModifiersState::CONTROL,
            KeyboardModes::DISAMBIGUATE_ESC_CODES | KeyboardModes::REPORT_ALTERNATE_KEYS,
            b"\x1b[1089::99;5u",
        );
        check(
            input,
            ModifiersState::CONTROL,
            KeyboardModes::DISAMBIGUATE_ESC_CODES,
            b"\x1b[1089;5u",
        );
        let upper = Key::Character("Ü".into());
        let lower = Key::Character("ü".into());
        let input = KeyInput {
            physical_key: PhysicalKey::Code(KeyCode::BracketLeft),
            ..key(&upper, &lower, Some("Ü"))
        };
        check(input, ModifiersState::SHIFT, KeyboardModes::all(), b"\x1b[252:220:91;2;220u");
        // Caps Lock's uppercase logical key must not accidentally become a
        // shifted alternate when there is no Shift modifier.
        check(input, ModifiersState::empty(), KeyboardModes::all(), b"\x1b[252::91;1;220u");
    }

    #[test]
    fn associated_text_has_complete_unicode_scalars_and_no_controls() {
        let ch = Key::Character("ü".into());
        let input = key(&ch, &ch, Some("a\u{308}😀"));
        check(
            input,
            ModifiersState::empty(),
            KeyboardModes::all(),
            b"\x1b[252::97;1;97:776:128512u",
        );
        for text in ["\x01", "a\x03", "\u{85}", "ü\x7f", ""] {
            check(
                KeyInput { text: Some(text), ..input },
                ModifiersState::empty(),
                KeyboardModes::REPORT_ALL_KEYS_AS_ESC | KeyboardModes::REPORT_ASSOCIATED_TEXT,
                b"\x1b[252u",
            );
        }
        check(
            KeyInput { state: ElementState::Released, ..input },
            ModifiersState::empty(),
            KeyboardModes::all(),
            b"\x1b[252::97;1:3u",
        );
    }

    #[test]
    fn composed_text_is_not_truncated_to_first_character() {
        let composed = Key::Character("a\u{308}".into());
        let input = key(&composed, &composed, Some("a\u{308}"));
        check(input, ModifiersState::empty(), KeyboardModes::all(), b"\x1b[0;1;97:776u");
        assert_eq!(
            translate(input, ModifiersState::empty(), KeyboardModes::REPORT_ALL_KEYS_AS_ESC),
            None
        );
        let dead = Key::Dead(Some('¨'));
        assert_eq!(
            translate(key(&dead, &composed, None), ModifiersState::empty(), KeyboardModes::all()),
            None
        );
    }

    #[test]
    fn keypad_distinguishes_navigation_digits_and_locale_decimal() {
        for (logical, physical, expected) in [
            (Key::Character("0".into()), KeyCode::Numpad0, "\x1b[57399u"),
            (Key::Character("9".into()), KeyCode::Numpad9, "\x1b[57408u"),
            (Key::Character(",".into()), KeyCode::NumpadDecimal, "\x1b[57409u"),
            (Key::Character("+".into()), KeyCode::NumpadAdd, "\x1b[57413u"),
            (Key::Named(NamedKey::Enter), KeyCode::NumpadEnter, "\x1b[57414u"),
            (Key::Named(NamedKey::ArrowLeft), KeyCode::Numpad4, "\x1b[57417u"),
            (Key::Named(NamedKey::Home), KeyCode::Numpad7, "\x1b[57423u"),
            (Key::Named(NamedKey::Delete), KeyCode::NumpadDecimal, "\x1b[57426u"),
            (Key::Named(NamedKey::Clear), KeyCode::Numpad5, "\x1b[E"),
        ] {
            let input = KeyInput {
                physical_key: PhysicalKey::Code(physical),
                location: KeyLocation::Numpad,
                ..key_input(&logical)
            };
            check(
                input,
                ModifiersState::empty(),
                KeyboardModes::DISAMBIGUATE_ESC_CODES,
                expected.as_bytes(),
            );
        }
    }

    #[test]
    fn keypad_text_and_actions_follow_requested_disambiguation() {
        let logical = Key::Character("1".into());
        let input = KeyInput {
            physical_key: PhysicalKey::Code(KeyCode::Numpad1),
            location: KeyLocation::Numpad,
            ..key(&logical, &logical, Some("1"))
        };
        check(input, ModifiersState::empty(), KeyboardModes::REPORT_EVENT_TYPES, b"1");
        check(
            KeyInput { state: ElementState::Released, text: None, ..input },
            ModifiersState::empty(),
            KeyboardModes::REPORT_EVENT_TYPES,
            b"\x1b[49;1:3u",
        );
        check(input, ModifiersState::empty(), KeyboardModes::all(), b"\x1b[57400;1;49u");
        check(input, ModifiersState::SHIFT, KeyboardModes::all(), b"\x1b[57400;2;49u");
        check(
            KeyInput { repeat: true, ..input },
            ModifiersState::empty(),
            KeyboardModes::all(),
            b"\x1b[57400;1:2;49u",
        );
        check(
            KeyInput { state: ElementState::Released, text: None, ..input },
            ModifiersState::empty(),
            KeyboardModes::all(),
            b"\x1b[57400;1:3u",
        );
    }

    #[test]
    fn modifier_events_use_side_and_supplied_post_event_modifier_state() {
        for (named, left, right, mods) in [
            (NamedKey::Shift, 57441, 57447, ModifiersState::SHIFT),
            (NamedKey::Control, 57442, 57448, ModifiersState::CONTROL),
            (NamedKey::Alt, 57443, 57449, ModifiersState::ALT),
            (NamedKey::Super, 57444, 57450, ModifiersState::SUPER),
        ] {
            let logical = Key::Named(named);
            let input = KeyInput { location: KeyLocation::Left, ..key_input(&logical) };
            assert_eq!(translate(input, mods, KeyboardModes::DISAMBIGUATE_ESC_CODES), None);
            let expected = format!(
                "\x1b[{left};{}u",
                match named {
                    NamedKey::Shift => 2,
                    NamedKey::Alt => 3,
                    NamedKey::Control => 5,
                    _ => 9,
                }
            );
            check(input, mods, KeyboardModes::all(), expected.as_bytes());
            let released =
                KeyInput { state: ElementState::Released, location: KeyLocation::Right, ..input };
            check(
                released,
                ModifiersState::empty(),
                KeyboardModes::all(),
                format!("\x1b[{right};1:3u").as_bytes(),
            );
            // Releasing one side while the other remains pressed retains it.
            check(
                released,
                mods,
                KeyboardModes::all(),
                format!(
                    "\x1b[{right};{}:3u",
                    match named {
                        NamedKey::Shift => 2,
                        NamedKey::Alt => 3,
                        NamedKey::Control => 5,
                        _ => 9,
                    }
                )
                .as_bytes(),
            );
        }
    }

    #[test]
    fn ime_commits_follow_text_reporting_flags() {
        check_text("a\u{308}😀", KeyboardModes::empty(), Some("a\u{308}😀".as_bytes()));
        check_text(
            "a\u{308}😀",
            KeyboardModes::REPORT_ALL_KEYS_AS_ESC,
            Some("a\u{308}😀".as_bytes()),
        );
        check_text("a\u{308}😀", KeyboardModes::all(), Some(b"\x1b[0;1;97:776:128512u"));
        check_text("a\x03", KeyboardModes::all(), None);
        check_text("", KeyboardModes::all(), None);
        for bits in 0..32 {
            let flags = KeyboardModes::from_bits_retain(bits);
            let expected: &[u8] = if bits & 24 == 24 {
                b"\x1b[0;1;26085:26412:35486u"
            } else {
                "日本語".as_bytes()
            };
            check_text("日本語", flags, Some(expected));
        }
    }

    fn check_text(text: &str, modes: KeyboardModes, expected: Option<&[u8]>) {
        assert_eq!(translate_text(text, modes), expected.map(<[u8]>::to_vec));
    }

    #[test]
    #[ignore = "manual input encoder benchmark; run serially with --release --ignored --nocapture"]
    fn benchmark_keyboard_encoding() {
        let logical = Key::Character("a".into());
        let input = key(&logical, &logical, Some("a"));
        const EVENTS: usize = 500_000;
        for (name, modes) in [
            ("legacy", KeyboardModes::empty()),
            ("kitty_disambiguated", KeyboardModes::DISAMBIGUATE_ESC_CODES),
            ("kitty_all", KeyboardModes::all()),
        ] {
            for _ in 0..10_000 {
                black_box(translate_input_with_modes(
                    black_box(input),
                    black_box(ModifiersState::CONTROL),
                    false,
                    black_box(modes),
                ));
            }
            let start = Instant::now();
            let mut bytes = 0;
            for _ in 0..EVENTS {
                bytes += black_box(translate_input_with_modes(
                    black_box(input),
                    black_box(ModifiersState::CONTROL),
                    false,
                    black_box(modes),
                ))
                .map_or(0, |sequence| sequence.len());
            }
            let elapsed = start.elapsed();
            eprintln!(
                "keyboard_encoding {name}: events={EVENTS} bytes={bytes} elapsed_us={} ns_per_event={:.1}",
                elapsed.as_micros(),
                elapsed.as_nanos() as f64 / EVENTS as f64
            );
        }
    }
}
