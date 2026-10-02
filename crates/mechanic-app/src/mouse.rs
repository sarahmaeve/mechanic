//! Mouse-event encoding for PTY forwarding.

use winit::keyboard::ModifiersState;

/// Physical button identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

/// What kind of event we're encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEventKind {
    Press,
    Release,
    /// Motion with the supplied button code.
    Motion,
}

/// Build the `Cb` byte used in both SGR and X10 framings.
fn encode_button(button: MouseButton, modifiers: ModifiersState, kind: MouseEventKind) -> u32 {
    let mut cb: u32 = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
    };

    if modifiers.shift_key() {
        cb += 4;
    }
    if modifiers.alt_key() {
        cb += 8;
    }
    if modifiers.control_key() {
        cb += 16;
    }

    if kind == MouseEventKind::Motion {
        cb += 32;
    }

    cb
}

/// Encode SGR mouse reporting (DECSET 1006) with 1-based coordinates.
pub fn encode_sgr(
    button: MouseButton,
    modifiers: ModifiersState,
    kind: MouseEventKind,
    col: u32,
    row: u32,
) -> Vec<u8> {
    let cb = encode_button(button, modifiers, kind);
    let terminator = if kind == MouseEventKind::Release { 'm' } else { 'M' };
    format!("\x1b[<{cb};{col};{row}{terminator}").into_bytes()
}

/// Encode legacy mouse reporting with 1-based coordinates, capped at 223.
pub fn encode_x10(
    button: MouseButton,
    modifiers: ModifiersState,
    kind: MouseEventKind,
    col: u32,
    row: u32,
) -> Vec<u8> {
    let cb = if kind == MouseEventKind::Release {
        let mut b: u32 = 3;
        if modifiers.shift_key() {
            b += 4;
        }
        if modifiers.alt_key() {
            b += 8;
        }
        if modifiers.control_key() {
            b += 16;
        }
        b
    } else {
        encode_button(button, modifiers, kind)
    };

    let cb_byte = (cb + 0x20).min(0xFF) as u8;
    let cx_byte = (col.saturating_add(0x20)).min(0xFF) as u8;
    let cy_byte = (row.saturating_add(0x20)).min(0xFF) as u8;

    vec![0x1b, b'[', b'M', cb_byte, cx_byte, cy_byte]
}

/// Dispatch to the right encoder based on whether SGR mode is active.
pub fn encode(
    sgr: bool,
    button: MouseButton,
    modifiers: ModifiersState,
    kind: MouseEventKind,
    col: u32,
    row: u32,
) -> Vec<u8> {
    if sgr {
        encode_sgr(button, modifiers, kind, col, row)
    } else {
        encode_x10(button, modifiers, kind, col, row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sgr_left_press_no_mods() {
        let bytes =
            encode_sgr(MouseButton::Left, ModifiersState::empty(), MouseEventKind::Press, 10, 5);
        assert_eq!(bytes, b"\x1b[<0;10;5M");
    }

    #[test]
    fn sgr_left_release_lowercase_m() {
        let bytes =
            encode_sgr(MouseButton::Left, ModifiersState::empty(), MouseEventKind::Release, 10, 5);
        assert_eq!(bytes, b"\x1b[<0;10;5m");
    }

    #[test]
    fn sgr_middle_and_right_button_numbers() {
        let m =
            encode_sgr(MouseButton::Middle, ModifiersState::empty(), MouseEventKind::Press, 1, 1);
        assert_eq!(m, b"\x1b[<1;1;1M");
        let r =
            encode_sgr(MouseButton::Right, ModifiersState::empty(), MouseEventKind::Press, 1, 1);
        assert_eq!(r, b"\x1b[<2;1;1M");
    }

    #[test]
    fn sgr_shift_modifier_adds_4() {
        let bytes =
            encode_sgr(MouseButton::Left, ModifiersState::SHIFT, MouseEventKind::Press, 1, 1);
        assert_eq!(bytes, b"\x1b[<4;1;1M");
    }

    #[test]
    fn sgr_alt_modifier_adds_8() {
        let bytes = encode_sgr(MouseButton::Left, ModifiersState::ALT, MouseEventKind::Press, 1, 1);
        assert_eq!(bytes, b"\x1b[<8;1;1M");
    }

    #[test]
    fn sgr_control_modifier_adds_16() {
        let bytes =
            encode_sgr(MouseButton::Left, ModifiersState::CONTROL, MouseEventKind::Press, 1, 1);
        assert_eq!(bytes, b"\x1b[<16;1;1M");
    }

    #[test]
    fn sgr_all_modifiers_stack() {
        let mods = ModifiersState::SHIFT | ModifiersState::ALT | ModifiersState::CONTROL;
        let bytes = encode_sgr(MouseButton::Right, mods, MouseEventKind::Press, 1, 1);
        assert_eq!(bytes, b"\x1b[<30;1;1M"); // 2 (right) + 4 + 8 + 16
    }

    #[test]
    fn sgr_drag_sets_motion_bit() {
        let bytes =
            encode_sgr(MouseButton::Left, ModifiersState::empty(), MouseEventKind::Motion, 10, 5);
        assert_eq!(bytes, b"\x1b[<32;10;5M");
    }

    #[test]
    fn sgr_wheel_up_uses_64() {
        let bytes =
            encode_sgr(MouseButton::WheelUp, ModifiersState::empty(), MouseEventKind::Press, 10, 5);
        assert_eq!(bytes, b"\x1b[<64;10;5M");
    }

    #[test]
    fn sgr_wheel_down_uses_65() {
        let bytes = encode_sgr(
            MouseButton::WheelDown,
            ModifiersState::empty(),
            MouseEventKind::Press,
            10,
            5,
        );
        assert_eq!(bytes, b"\x1b[<65;10;5M");
    }

    #[test]
    fn sgr_wheel_with_shift_adds_4() {
        let bytes =
            encode_sgr(MouseButton::WheelUp, ModifiersState::SHIFT, MouseEventKind::Press, 1, 1);
        assert_eq!(bytes, b"\x1b[<68;1;1M"); // 64 + 4
    }

    #[test]
    fn sgr_large_coordinates_not_truncated() {
        let bytes =
            encode_sgr(MouseButton::Left, ModifiersState::empty(), MouseEventKind::Press, 400, 100);
        assert_eq!(bytes, b"\x1b[<0;400;100M");
    }

    #[test]
    fn x10_left_press_at_one_one() {
        let bytes =
            encode_x10(MouseButton::Left, ModifiersState::empty(), MouseEventKind::Press, 1, 1);
        assert_eq!(bytes, b"\x1b[M !!");
    }

    #[test]
    fn x10_release_uses_button_3() {
        let bytes =
            encode_x10(MouseButton::Left, ModifiersState::empty(), MouseEventKind::Release, 1, 1);
        assert_eq!(bytes, b"\x1b[M#!!");
    }

    #[test]
    fn x10_right_press() {
        let bytes =
            encode_x10(MouseButton::Right, ModifiersState::empty(), MouseEventKind::Press, 1, 1);
        assert_eq!(bytes, b"\x1b[M\"!!");
    }

    #[test]
    fn x10_coords_clamped_at_223() {
        let bytes =
            encode_x10(MouseButton::Left, ModifiersState::empty(), MouseEventKind::Press, 300, 300);
        assert_eq!(bytes, b"\x1b[M \xff\xff");
    }

    #[test]
    fn x10_wheel_events_are_framed() {
        let bytes =
            encode_x10(MouseButton::WheelUp, ModifiersState::empty(), MouseEventKind::Press, 5, 5);
        assert_eq!(bytes, b"\x1b[M`%%"); // '`' is 0x60, '%' is 0x25 (5+32)
    }

    #[test]
    fn dispatch_sgr_true_routes_to_sgr() {
        let bytes =
            encode(true, MouseButton::Left, ModifiersState::empty(), MouseEventKind::Press, 10, 5);
        assert_eq!(bytes, b"\x1b[<0;10;5M");
    }

    #[test]
    fn dispatch_sgr_false_routes_to_x10() {
        let bytes =
            encode(false, MouseButton::Left, ModifiersState::empty(), MouseEventKind::Press, 1, 1);
        assert_eq!(bytes, b"\x1b[M !!");
    }
}
