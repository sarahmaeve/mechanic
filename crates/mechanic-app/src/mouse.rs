//! Mouse-event encoding for PTY forwarding.
//!
//! When the running program has enabled mouse reporting via DECSET
//! 1000/1002/1003 (optionally with 1006 for SGR encoding), the
//! terminal forwards every relevant mouse event to the PTY as an
//! escape sequence.  This module is the pure codec for those
//! sequences.
//!
//! # Two wire formats
//!
//! - **SGR** (DECSET 1006) — `\x1b[<Cb;Cx;CyM` for press/drag/wheel,
//!   `\x1b[<Cb;Cx;Cym` for release (lowercase `m`).  `Cb` encodes
//!   button + modifiers + motion-flag + wheel-flag (see
//!   [`encode_button`]).  Coordinates are 1-based decimal numbers, no
//!   practical upper bound.  Every modern TUI (vim, tmux, fzf, less,
//!   nvim, tig, …) prefers this.
//!
//! - **Legacy X10** — `\x1b[M <Cb> <Cx> <Cy>` where each is a single
//!   byte equal to `value + 0x20`.  Used when 1000/1002/1003 is set
//!   without 1006.  Coordinates cap at 223 (255 − 32).  Kept for
//!   compatibility with very old programs.
//!
//! # Button numbers (both formats share this encoding)
//!
//! | Value | Meaning                        |
//! |------:|--------------------------------|
//! |   `0` | Left button                    |
//! |   `1` | Middle button                  |
//! |   `2` | Right button                   |
//! |   `3` | Release (X10 only — SGR uses a lowercase `m` terminator instead) |
//! |  `+4` | Shift modifier                 |
//! |  `+8` | Meta / Alt modifier            |
//! | `+16` | Control modifier               |
//! | `+32` | Motion (drag) flag             |
//! | `+64` | Wheel up                       |
//! | `+65` | Wheel down                     |

use winit::keyboard::ModifiersState;

// ── Public types ──────────────────────────────────────────────────────────────

/// Physical button identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    /// Scroll wheel scrolled toward the user (line-up).
    WheelUp,
    /// Scroll wheel scrolled away from the user (line-down).
    WheelDown,
}

/// What kind of event we're encoding.
///
/// `Motion` is a drag event — a motion while a button is held.  Plain
/// motion (no button) is encoded the same way but with the button
/// field set to `3` (released) + the motion flag; it's only forwarded
/// when DECSET 1003 is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEventKind {
    /// Button was pressed.
    Press,
    /// Button was released.
    Release,
    /// Motion — either a drag (button held) or free motion.
    Motion,
}

// ── Encoding ──────────────────────────────────────────────────────────────────

/// Build the `Cb` byte used in both SGR and X10 framings.
///
/// Not exposed directly — `encode_sgr` and `encode_x10` use it.  Kept
/// as a pure function so its bit-math can be exercised in isolation.
fn encode_button(
    button: MouseButton,
    modifiers: ModifiersState,
    kind: MouseEventKind,
) -> u32 {
    // Base button number.  Wheel events skip the 0-2 range entirely.
    let mut cb: u32 = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
    };

    // Modifier bits.  Shift=+4, Meta(Alt)=+8, Control=+16.
    if modifiers.shift_key() {
        cb += 4;
    }
    if modifiers.alt_key() {
        cb += 8;
    }
    if modifiers.control_key() {
        cb += 16;
    }

    // Motion flag: set for drag events regardless of button.
    if kind == MouseEventKind::Motion {
        cb += 32;
    }

    cb
}

/// Encode a mouse event using the SGR framing (DECSET 1006).
///
/// `col` and `row` are **1-based** grid coordinates (the wire format
/// expects 1-based).  Callers converting from 0-based internal indices
/// must add 1 beforehand.
///
/// Returns a byte vector that the caller writes to the PTY.
///
/// ```text
/// SGR press:   ESC [ < Cb ; Cx ; Cy M
/// SGR release: ESC [ < Cb ; Cx ; Cy m
/// ```
pub fn encode_sgr(
    button: MouseButton,
    modifiers: ModifiersState,
    kind: MouseEventKind,
    col: u32,
    row: u32,
) -> Vec<u8> {
    let cb = encode_button(button, modifiers, kind);
    // Lowercase `m` for Release, uppercase `M` otherwise.  Wheel events
    // use uppercase (they have no "release" — each tick is a `M`).
    let terminator = if kind == MouseEventKind::Release { 'm' } else { 'M' };
    format!("\x1b[<{cb};{col};{row}{terminator}").into_bytes()
}

/// Encode a mouse event using the legacy X10 framing.
///
/// Used when the program enabled 1000/1002/1003 but NOT 1006.
/// Coordinates are clamped to `1..=223` (255 − 32) because the wire
/// format puts each value in a single byte offset by 0x20.  Events
/// outside the representable range silently clamp — the alternative
/// is to drop them, which loses more information.
///
/// The Release kind is encoded by setting the button field to `3`
/// regardless of which button was actually released: X10 can't
/// distinguish which button came up, a known limitation that's one
/// reason SGR exists.
///
/// ```text
/// X10: ESC [ M Cb Cx Cy      (each value = actual + 0x20)
/// ```
pub fn encode_x10(
    button: MouseButton,
    modifiers: ModifiersState,
    kind: MouseEventKind,
    col: u32,
    row: u32,
) -> Vec<u8> {
    let cb = if kind == MouseEventKind::Release {
        // X10 release uses button=3 + modifiers (no motion flag on
        // release in this framing).
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

    // Offset by 0x20 and clamp to the single-byte range.  `cb` for
    // wheel events is already ≤ 65 + modifier bits + motion = well
    // under the cap, so this only bites exceptionally wide grids.
    let cb_byte = (cb + 0x20).min(0xFF) as u8;
    let cx_byte = (col.saturating_add(0x20)).min(0xFF) as u8;
    let cy_byte = (row.saturating_add(0x20)).min(0xFF) as u8;

    vec![0x1b, b'[', b'M', cb_byte, cx_byte, cy_byte]
}

/// Dispatch to the right encoder based on whether SGR mode is active.
///
/// The `Terminal::mouse_protocol()` accessor gives the caller the
/// `sgr` flag; feeding it through here keeps the call-site branch in
/// one place.
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── SGR framing ───────────────────────────────────────────────────────────

    #[test]
    fn sgr_left_press_no_mods() {
        // Canonical case: bare left-click at (col=10, row=5).
        let bytes = encode_sgr(
            MouseButton::Left,
            ModifiersState::empty(),
            MouseEventKind::Press,
            10,
            5,
        );
        assert_eq!(bytes, b"\x1b[<0;10;5M");
    }

    #[test]
    fn sgr_left_release_lowercase_m() {
        // Release switches ONLY the terminator to lowercase `m`; the
        // Cb field stays identical to the press.  This is the SGR
        // protocol's headline improvement over X10 (which couldn't
        // identify which button was released).
        let bytes = encode_sgr(
            MouseButton::Left,
            ModifiersState::empty(),
            MouseEventKind::Release,
            10,
            5,
        );
        assert_eq!(bytes, b"\x1b[<0;10;5m");
    }

    #[test]
    fn sgr_middle_and_right_button_numbers() {
        let m = encode_sgr(
            MouseButton::Middle,
            ModifiersState::empty(),
            MouseEventKind::Press,
            1,
            1,
        );
        assert_eq!(m, b"\x1b[<1;1;1M");
        let r = encode_sgr(
            MouseButton::Right,
            ModifiersState::empty(),
            MouseEventKind::Press,
            1,
            1,
        );
        assert_eq!(r, b"\x1b[<2;1;1M");
    }

    #[test]
    fn sgr_shift_modifier_adds_4() {
        let bytes = encode_sgr(
            MouseButton::Left,
            ModifiersState::SHIFT,
            MouseEventKind::Press,
            1,
            1,
        );
        assert_eq!(bytes, b"\x1b[<4;1;1M");
    }

    #[test]
    fn sgr_alt_modifier_adds_8() {
        let bytes = encode_sgr(
            MouseButton::Left,
            ModifiersState::ALT,
            MouseEventKind::Press,
            1,
            1,
        );
        assert_eq!(bytes, b"\x1b[<8;1;1M");
    }

    #[test]
    fn sgr_control_modifier_adds_16() {
        let bytes = encode_sgr(
            MouseButton::Left,
            ModifiersState::CONTROL,
            MouseEventKind::Press,
            1,
            1,
        );
        assert_eq!(bytes, b"\x1b[<16;1;1M");
    }

    #[test]
    fn sgr_all_modifiers_stack() {
        // 4 + 8 + 16 = 28.  Useful for exotic TUI bindings (ctrl-alt-shift-click).
        let mods = ModifiersState::SHIFT | ModifiersState::ALT | ModifiersState::CONTROL;
        let bytes = encode_sgr(MouseButton::Right, mods, MouseEventKind::Press, 1, 1);
        assert_eq!(bytes, b"\x1b[<30;1;1M"); // 2 (right) + 4 + 8 + 16
    }

    #[test]
    fn sgr_drag_sets_motion_bit() {
        // Motion flag adds 32.  Left-drag → 0 + 32 = 32.
        let bytes = encode_sgr(
            MouseButton::Left,
            ModifiersState::empty(),
            MouseEventKind::Motion,
            10,
            5,
        );
        assert_eq!(bytes, b"\x1b[<32;10;5M");
    }

    #[test]
    fn sgr_wheel_up_uses_64() {
        // Wheel events have their own button range starting at 64.
        // Note: terminator is `M`, not `m` — wheel "events" don't have
        // a separate release.
        let bytes = encode_sgr(
            MouseButton::WheelUp,
            ModifiersState::empty(),
            MouseEventKind::Press,
            10,
            5,
        );
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
        // Shift-wheel: many TUIs interpret this as "scroll the pane,
        // not the buffer" or similar — the modifier passes through cleanly.
        let bytes = encode_sgr(
            MouseButton::WheelUp,
            ModifiersState::SHIFT,
            MouseEventKind::Press,
            1,
            1,
        );
        assert_eq!(bytes, b"\x1b[<68;1;1M"); // 64 + 4
    }

    #[test]
    fn sgr_large_coordinates_not_truncated() {
        // A big grid (e.g. 400 cols × 100 rows on a 4K display) works
        // because SGR uses decimal, not byte-packed encoding.
        let bytes = encode_sgr(
            MouseButton::Left,
            ModifiersState::empty(),
            MouseEventKind::Press,
            400,
            100,
        );
        assert_eq!(bytes, b"\x1b[<0;400;100M");
    }

    // ── X10 framing ───────────────────────────────────────────────────────────

    #[test]
    fn x10_left_press_at_one_one() {
        // Canonical test vector for the legacy encoder.
        // cb = 0 + 32 = 0x20 = ' '
        // cx = 1 + 32 = 0x21 = '!'
        // cy = 1 + 32 = 0x21 = '!'
        let bytes = encode_x10(
            MouseButton::Left,
            ModifiersState::empty(),
            MouseEventKind::Press,
            1,
            1,
        );
        assert_eq!(bytes, b"\x1b[M !!");
    }

    #[test]
    fn x10_release_uses_button_3() {
        // X10 can't tell which button was released; all releases
        // encode as button=3.  Confirm that.
        let bytes = encode_x10(
            MouseButton::Left,
            ModifiersState::empty(),
            MouseEventKind::Release,
            1,
            1,
        );
        // cb = 3 + 32 = 0x23 = '#'
        assert_eq!(bytes, b"\x1b[M#!!");
    }

    #[test]
    fn x10_right_press() {
        let bytes = encode_x10(
            MouseButton::Right,
            ModifiersState::empty(),
            MouseEventKind::Press,
            1,
            1,
        );
        // cb = 2 + 32 = 0x22 = '"'
        assert_eq!(bytes, b"\x1b[M\"!!");
    }

    #[test]
    fn x10_coords_clamped_at_223() {
        // Any column beyond 223 gets clamped; otherwise the byte
        // (col + 32) would wrap into random control characters.
        let bytes = encode_x10(
            MouseButton::Left,
            ModifiersState::empty(),
            MouseEventKind::Press,
            300,
            300,
        );
        // cb = 32 (' '), cx = 255 (0xFF), cy = 255 (0xFF)
        assert_eq!(bytes, b"\x1b[M \xff\xff");
    }

    #[test]
    fn x10_wheel_events_are_framed() {
        // Wheel up: cb = 64, wire byte = 64+32 = 96 = '`'
        let bytes = encode_x10(
            MouseButton::WheelUp,
            ModifiersState::empty(),
            MouseEventKind::Press,
            5,
            5,
        );
        assert_eq!(bytes, b"\x1b[M`%%"); // '`' is 0x60, '%' is 0x25 (5+32)
    }

    // ── Dispatch ──────────────────────────────────────────────────────────────

    #[test]
    fn dispatch_sgr_true_routes_to_sgr() {
        let bytes = encode(
            true,
            MouseButton::Left,
            ModifiersState::empty(),
            MouseEventKind::Press,
            10,
            5,
        );
        assert_eq!(bytes, b"\x1b[<0;10;5M");
    }

    #[test]
    fn dispatch_sgr_false_routes_to_x10() {
        let bytes = encode(
            false,
            MouseButton::Left,
            ModifiersState::empty(),
            MouseEventKind::Press,
            1,
            1,
        );
        assert_eq!(bytes, b"\x1b[M !!");
    }
}
