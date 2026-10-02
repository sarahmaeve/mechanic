//! Mouse-event encoding for PTY forwarding.

use winit::event::{MouseScrollDelta, TouchPhase};
use winit::keyboard::ModifiersState;

/// Physical button identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    /// No button held (DECSET 1003 hover motion).
    None,
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
        MouseButton::None => 3,
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

/// Buttons currently held, with the most recently pressed button used for motion.
#[derive(Debug, Default)]
pub struct HeldButtons(Vec<MouseButton>);

impl HeldButtons {
    pub fn update(&mut self, button: MouseButton, pressed: bool) {
        self.0.retain(|held| *held != button);
        if pressed && matches!(button, MouseButton::Left | MouseButton::Middle | MouseButton::Right)
        {
            self.0.push(button);
        }
    }

    pub fn motion_button(&self) -> MouseButton {
        self.0.last().copied().unwrap_or(MouseButton::None)
    }

    pub fn report_button(&self, report_motion: bool, report_drag: bool) -> Option<MouseButton> {
        let button = self.motion_button();
        (report_motion || (report_drag && button != MouseButton::None)).then_some(button)
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

/// Retain sub-line wheel deltas within one gesture and destination.
#[derive(Debug, Default)]
pub struct ScrollAccumulator {
    remainder: f64,
    context: Option<(Option<bool>, ModifiersState, bool, u32)>,
}

impl ScrollAccumulator {
    pub fn reset(&mut self) {
        self.remainder = 0.0;
        self.context = None;
    }

    pub fn lines(
        &mut self,
        delta: MouseScrollDelta,
        phase: TouchPhase,
        route: Option<bool>,
        modifiers: ModifiersState,
        cell_height: f32,
    ) -> i32 {
        if matches!(phase, TouchPhase::Started | TouchPhase::Cancelled) {
            self.reset();
        }
        if phase == TouchPhase::Cancelled {
            return 0;
        }
        let height = cell_height.max(1.0);
        let (lines, pixels) = match delta {
            MouseScrollDelta::LineDelta(_, y) => (f64::from(y), false),
            MouseScrollDelta::PixelDelta(pos) => (pos.y / f64::from(height), true),
        };
        // Local scrolling ignores modifiers; reports encode them into the button byte.
        let modifiers = if route.is_some() { modifiers } else { ModifiersState::empty() };
        let context = (route, modifiers, pixels, if pixels { height.to_bits() } else { 0 });
        if self.context != Some(context) {
            self.reset();
            self.context = Some(context);
        }
        if !lines.is_finite() {
            self.reset();
            return 0;
        }
        self.remainder += lines;
        let whole = self.remainder.trunc() as i32;
        self.remainder -= f64::from(whole);
        if phase == TouchPhase::Ended {
            self.reset();
        }
        whole
    }
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
    use winit::dpi::PhysicalPosition;

    #[test]
    fn hover_reports_no_button_in_both_encodings() {
        assert_eq!(
            encode_sgr(MouseButton::None, ModifiersState::empty(), MouseEventKind::Motion, 10, 5),
            b"\x1b[<35;10;5M"
        );
        assert_eq!(
            encode_x10(MouseButton::None, ModifiersState::CONTROL, MouseEventKind::Motion, 1, 1),
            b"\x1b[MS!!"
        );
    }

    #[test]
    fn motion_tracks_held_buttons_and_falls_back_after_release() {
        let mut held = HeldButtons::default();
        assert_eq!(held.motion_button(), MouseButton::None);
        held.update(MouseButton::Right, true);
        assert_eq!(held.motion_button(), MouseButton::Right);
        held.update(MouseButton::Middle, true);
        assert_eq!(held.motion_button(), MouseButton::Middle);
        held.update(MouseButton::Middle, false);
        assert_eq!(held.motion_button(), MouseButton::Right);
        held.update(MouseButton::Left, false);
        assert_eq!(held.motion_button(), MouseButton::Right);
        held.clear();
        assert_eq!(held.motion_button(), MouseButton::None);
    }

    #[test]
    fn drag_mode_requires_a_held_button_and_all_motion_mode_reports_hover() {
        let mut held = HeldButtons::default();
        assert_eq!(held.report_button(false, true), None);
        assert_eq!(held.report_button(true, false), Some(MouseButton::None));
        held.update(MouseButton::Right, true);
        assert_eq!(held.report_button(false, true), Some(MouseButton::Right));
        assert_eq!(held.report_button(false, false), None);
        held.update(MouseButton::Middle, true);
        assert_eq!(held.report_button(true, false), Some(MouseButton::Middle));
    }

    fn pixels(y: f64) -> MouseScrollDelta {
        MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, y))
    }

    fn scroll(accumulator: &mut ScrollAccumulator, y: f64, route: Option<bool>) -> i32 {
        accumulator.lines(pixels(y), TouchPhase::Moved, route, ModifiersState::empty(), 10.0)
    }

    #[test]
    fn pixel_scroll_preserves_fraction_and_signed_remainder() {
        let mut accumulator = ScrollAccumulator::default();
        assert_eq!(scroll(&mut accumulator, 4.0, None), 0);
        assert_eq!(scroll(&mut accumulator, 9.0, None), 1);
        assert_eq!(scroll(&mut accumulator, -5.0, None), 0);
        assert_eq!(scroll(&mut accumulator, -9.0, None), -1);
        assert_eq!(scroll(&mut accumulator, -9.0, None), -1);
    }

    #[test]
    fn fractional_line_scroll_is_accumulated() {
        let mut accumulator = ScrollAccumulator::default();
        for expected in [0, 0, 0, 1] {
            assert_eq!(
                accumulator.lines(
                    MouseScrollDelta::LineDelta(0.0, 0.25),
                    TouchPhase::Moved,
                    Some(true),
                    ModifiersState::empty(),
                    10.0,
                ),
                expected
            );
        }
    }

    #[test]
    fn scroll_remainder_does_not_cross_local_reported_or_encoding_routes() {
        let mut accumulator = ScrollAccumulator::default();
        assert_eq!(scroll(&mut accumulator, 8.0, None), 0);
        assert_eq!(scroll(&mut accumulator, 3.0, Some(true)), 0);
        assert_eq!(scroll(&mut accumulator, 8.0, Some(true)), 1);
        assert_eq!(scroll(&mut accumulator, 9.0, Some(false)), 0);
        assert_eq!(scroll(&mut accumulator, 2.0, None), 0);
    }

    #[test]
    fn scroll_gesture_boundaries_and_cancellation_reset_remainder() {
        let mut accumulator = ScrollAccumulator::default();
        assert_eq!(scroll(&mut accumulator, 8.0, None), 0);
        assert_eq!(
            accumulator.lines(
                pixels(4.0),
                TouchPhase::Started,
                None,
                ModifiersState::empty(),
                10.0
            ),
            0
        );
        assert_eq!(
            accumulator.lines(pixels(7.0), TouchPhase::Ended, None, ModifiersState::empty(), 10.0),
            1
        );
        assert_eq!(scroll(&mut accumulator, 9.0, None), 0);
        assert_eq!(
            accumulator.lines(
                pixels(20.0),
                TouchPhase::Cancelled,
                None,
                ModifiersState::empty(),
                10.0
            ),
            0
        );
        assert_eq!(scroll(&mut accumulator, 2.0, None), 0);
    }

    #[test]
    fn scroll_unit_and_cell_height_changes_reset_remainder() {
        let mut accumulator = ScrollAccumulator::default();
        assert_eq!(scroll(&mut accumulator, 8.0, None), 0);
        assert_eq!(
            accumulator.lines(
                MouseScrollDelta::LineDelta(0.0, 0.5),
                TouchPhase::Moved,
                None,
                ModifiersState::empty(),
                10.0
            ),
            0
        );
        assert_eq!(scroll(&mut accumulator, 8.0, None), 0);
        assert_eq!(
            accumulator.lines(pixels(8.0), TouchPhase::Moved, None, ModifiersState::empty(), 20.0),
            0
        );
        accumulator.reset();
        assert_eq!(scroll(&mut accumulator, 8.0, None), 0);
    }

    #[test]
    fn changed_report_modifiers_do_not_consume_previous_fraction() {
        let mut accumulator = ScrollAccumulator::default();
        assert_eq!(scroll(&mut accumulator, 8.0, Some(true)), 0);
        assert_eq!(
            accumulator.lines(
                pixels(3.0),
                TouchPhase::Moved,
                Some(true),
                ModifiersState::CONTROL,
                10.0
            ),
            0
        );
        assert_eq!(
            accumulator.lines(
                pixels(8.0),
                TouchPhase::Moved,
                Some(true),
                ModifiersState::CONTROL,
                10.0
            ),
            1
        );
    }

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
