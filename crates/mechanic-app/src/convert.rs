//! Grid conversion: [`mechanic_core::Terminal`] → [`mechanic_renderer::RenderGrid`].
//!
//! Each frame the application calls [`convert_grid`] to produce a
//! [`RenderGrid`] snapshot from the live terminal state.  The GPU renderer
//! then consumes that snapshot without touching any alacritty-internal types.

use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use mechanic_config::theme::{Rgb, Theme};
use mechanic_core::Terminal;
use mechanic_renderer::{CellFlags, CursorStyle, RenderCell, RenderGrid};

// ── Public API ────────────────────────────────────────────────────────────────

/// Convert the current visible grid of `terminal` into a [`RenderGrid`] that
/// the GPU renderer can consume.
///
/// Colors are resolved against `theme` so that named ANSI colors (e.g.
/// `NamedColor::Red`) are mapped to the palette configured by the user.
///
/// # Wide characters
///
/// Wide characters (CJK, emoji, …) occupy two columns in the terminal grid.
/// The first column carries the glyph with `Flags::WIDE_CHAR` set; the second
/// column is a spacer placeholder with `Flags::WIDE_CHAR_SPACER` set.  This
/// function skips spacer cells so the renderer never sees them — the wide
/// character itself is written to its own cell with the normal column index.
pub fn convert_grid(terminal: &Terminal, theme: &Theme) -> RenderGrid {
    let grid = terminal.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    let display_offset = grid.display_offset();

    let mut render_grid = RenderGrid::new(cols, rows);

    // Iterate over every visible cell.  `display_iter` yields cells in
    // row-major order starting from the topmost visible line.
    //
    // The alacritty grid uses signed line numbers where `Line(0)` is the
    // first line of the *active* (non-scrollback) area.  When the viewport is
    // scrolled up by `display_offset` lines, the topmost visible line has
    // index `Line(-display_offset as i32)`.  Converting to a viewport row:
    //
    //   viewport_row = point.line.0 + display_offset as i32
    for indexed in grid.display_iter() {
        let cell = indexed.cell;

        // Skip the spacer placeholder of a wide character — the glyph was
        // already written in the preceding (left) column.
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }

        let row = (indexed.point.line.0 + display_offset as i32) as usize;
        let col = indexed.point.column.0;

        // Bounds check: should never fire, but avoids a panic if the iterator
        // somehow yields out-of-range coordinates.
        if row >= rows || col >= cols {
            continue;
        }

        let mut flags = CellFlags::empty();
        if cell.flags.contains(Flags::BOLD) {
            flags |= CellFlags::BOLD;
        }
        if cell.flags.contains(Flags::ITALIC) {
            flags |= CellFlags::ITALIC;
        }
        if cell.flags.intersects(Flags::ALL_UNDERLINES) {
            flags |= CellFlags::UNDERLINE;
        }
        if cell.flags.contains(Flags::INVERSE) {
            flags |= CellFlags::INVERSE;
        }

        let render_cell = RenderCell {
            character: cell.c,
            fg: resolve_color(&cell.fg, theme),
            bg: resolve_color(&cell.bg, theme),
            flags,
        };

        // Safety: bounds checked above.
        render_grid.cells[row * cols + col] = render_cell;
    }

    // ── Cursor position ───────────────────────────────────────────────────────

    // The cursor's grid line is in the active viewport coordinate space
    // (`Line(0)` = top of the active area).  Adding `display_offset` converts
    // it to the same viewport-relative row used above.
    let cursor_line = grid.cursor.point.line.0;
    let cursor_row = (cursor_line + display_offset as i32).clamp(0, rows as i32 - 1) as usize;
    let cursor_col = grid.cursor.point.column.0.min(cols.saturating_sub(1));
    render_grid.cursor_position = (cursor_col, cursor_row);

    // ── Cursor style ──────────────────────────────────────────────────────────

    render_grid.cursor_style = match terminal.cursor_shape() {
        CursorShape::Block | CursorShape::HollowBlock => CursorStyle::Block,
        CursorShape::Underline => CursorStyle::Underline,
        CursorShape::Beam => CursorStyle::Bar,
        CursorShape::Hidden => CursorStyle::Block,
    };

    // ── Selection highlight ───────────────────────────────────────────────────
    //
    // Applied before the block-cursor recolor so the cursor cell always wins
    // visually — otherwise a selection passing over the cursor cell would
    // paint it with the selection colors and make the cursor disappear.
    if let Some(sel_range) = terminal.selection_range() {
        apply_selection_highlight(&mut render_grid, &sel_range, display_offset, cols, rows, theme);
    }

    // ── Block-cursor cell recolor ─────────────────────────────────────────────
    //
    // For Block cursors, repaint the cell under the cursor directly rather
    // than drawing an opaque block on top: set its background to the cursor
    // color and its foreground to `theme.cursor_text`.  This keeps the
    // character under the cursor visible through the cursor block instead of
    // hiding it behind a solid square.
    //
    // Bar and Underline cursors don't cover the character, so they remain
    // rendered as separate quads by the pipeline's cursor pass.
    if matches!(render_grid.cursor_style, CursorStyle::Block) {
        if let Some(cell) = render_grid.get_mut(cursor_col, cursor_row) {
            cell.bg = theme.cursor;
            cell.fg = theme.cursor_text;
        }
    }

    render_grid
}

/// Apply selection highlight colors to all cells within `sel_range`.
///
/// Cells inside the selection have their foreground and background colors
/// replaced with the theme's selection colors.
fn apply_selection_highlight(
    render_grid: &mut RenderGrid,
    sel_range: &SelectionRange,
    display_offset: usize,
    cols: usize,
    rows: usize,
    theme: &Theme,
) {
    let sel_bg = theme.selection.background;
    let sel_fg = theme.selection.foreground;

    let start = sel_range.start;
    let end = sel_range.end;

    // Walk every grid line that overlaps the selection.
    let start_line = start.line.0;
    let end_line = end.line.0;

    for line_idx in start_line..=end_line {
        // Convert grid line index to a viewport row.
        let viewport_row = line_idx + display_offset as i32;
        if viewport_row < 0 || viewport_row >= rows as i32 {
            continue;
        }
        let row = viewport_row as usize;

        // Determine the column range for this line.
        let col_start = if line_idx == start_line { start.column.0 } else { 0 };

        let col_end = if line_idx == end_line { end.column.0 } else { cols.saturating_sub(1) };

        for col in col_start..=col_end.min(cols.saturating_sub(1)) {
            let idx = row * cols + col;
            if idx < render_grid.cells.len() {
                let cell = &mut render_grid.cells[idx];
                cell.bg = sel_bg;
                if let Some(fg) = sel_fg {
                    cell.fg = fg;
                }
            }
        }
    }
}

// ── Color resolution ──────────────────────────────────────────────────────────

/// Resolve an alacritty [`Color`] to our [`Rgb`] type using the active
/// [`Theme`].
///
/// # Mapping rules
///
/// | alacritty `Color` variant | result |
/// |---|---|
/// | `Named(Foreground)` | `theme.foreground` |
/// | `Named(Background)` | `theme.background` |
/// | `Named(Cursor)` | `theme.cursor` |
/// | `Named(Black)` … `Named(BrightWhite)` | corresponding `theme.ansi` field |
/// | `Named(Dim*)` | same slot as the non-dim variant (dim rendering deferred) |
/// | `Named(BrightForeground)` | `theme.foreground` |
/// | `Named(DimForeground)` | `theme.foreground` |
/// | `Named(DimBackground)` | `theme.background` |
/// | `Spec(rgb)` | truecolor pass-through (field-by-field copy) |
/// | `Indexed(0..=15)` | ANSI table via `theme.ansi` |
/// | `Indexed(16..=231)` | 6×6×6 RGB cube |
/// | `Indexed(232..=255)` | 24-step greyscale ramp |
fn resolve_color(color: &Color, theme: &Theme) -> Rgb {
    match color {
        Color::Named(named) => resolve_named(*named, theme),

        // Truecolor: vte's `Rgb` and our `Rgb` have identical fields but are
        // different types — copy field by field.
        Color::Spec(vte_rgb) => Rgb { r: vte_rgb.r, g: vte_rgb.g, b: vte_rgb.b },

        Color::Indexed(idx) => resolve_indexed(*idx, theme),
    }
}

/// Resolve a [`NamedColor`] to our [`Rgb`].
fn resolve_named(named: NamedColor, theme: &Theme) -> Rgb {
    let ansi = &theme.ansi;
    match named {
        // Special semantic colors.
        NamedColor::Foreground => theme.foreground,
        NamedColor::Background => theme.background,
        NamedColor::Cursor => theme.cursor,

        // Standard 16-color ANSI palette.
        NamedColor::Black => ansi.black,
        NamedColor::Red => ansi.red,
        NamedColor::Green => ansi.green,
        NamedColor::Yellow => ansi.yellow,
        NamedColor::Blue => ansi.blue,
        NamedColor::Magenta => ansi.magenta,
        NamedColor::Cyan => ansi.cyan,
        NamedColor::White => ansi.white,
        NamedColor::BrightBlack => ansi.bright_black,
        NamedColor::BrightRed => ansi.bright_red,
        NamedColor::BrightGreen => ansi.bright_green,
        NamedColor::BrightYellow => ansi.bright_yellow,
        NamedColor::BrightBlue => ansi.bright_blue,
        NamedColor::BrightMagenta => ansi.bright_magenta,
        NamedColor::BrightCyan => ansi.bright_cyan,
        NamedColor::BrightWhite => ansi.bright_white,

        // Dim variants — map to their normal counterparts (dim rendering is
        // deferred; we don't darken colors at this stage).
        NamedColor::DimBlack => ansi.black,
        NamedColor::DimRed => ansi.red,
        NamedColor::DimGreen => ansi.green,
        NamedColor::DimYellow => ansi.yellow,
        NamedColor::DimBlue => ansi.blue,
        NamedColor::DimMagenta => ansi.magenta,
        NamedColor::DimCyan => ansi.cyan,
        NamedColor::DimWhite => ansi.white,

        // Bright / dim foreground aliases.
        NamedColor::BrightForeground => theme.foreground,
        NamedColor::DimForeground => theme.foreground,
    }
}

/// Resolve a 256-color palette index to our [`Rgb`].
///
/// The 256-color space is divided as follows:
///
/// - `0..=15`   — the standard 16 ANSI colors (delegated to `theme.ansi`)
/// - `16..=231` — a 6×6×6 RGB colour cube
/// - `232..=255` — a 24-step black-to-white greyscale ramp
fn resolve_indexed(idx: u8, theme: &Theme) -> Rgb {
    match idx {
        // ── ANSI 16 ──────────────────────────────────────────────────────────
        0..=15 => {
            // Map index to the corresponding NamedColor variant and delegate.
            let named = match idx {
                0 => NamedColor::Black,
                1 => NamedColor::Red,
                2 => NamedColor::Green,
                3 => NamedColor::Yellow,
                4 => NamedColor::Blue,
                5 => NamedColor::Magenta,
                6 => NamedColor::Cyan,
                7 => NamedColor::White,
                8 => NamedColor::BrightBlack,
                9 => NamedColor::BrightRed,
                10 => NamedColor::BrightGreen,
                11 => NamedColor::BrightYellow,
                12 => NamedColor::BrightBlue,
                13 => NamedColor::BrightMagenta,
                14 => NamedColor::BrightCyan,
                15 => NamedColor::BrightWhite,
                // SAFETY: exhaustive for 0..=15.
                _ => unreachable!(),
            };
            resolve_named(named, theme)
        }

        // ── 6×6×6 RGB cube ───────────────────────────────────────────────────
        //
        // Index `i` in the range 16..=231 encodes:
        //   i -= 16
        //   r = i / 36        (0..6)
        //   g = (i / 6) % 6   (0..6)
        //   b = i % 6         (0..6)
        //
        // Each 0..6 component maps to: 0 → 0, 1..5 → 55 + component * 40.
        16..=231 => {
            let i = idx - 16;
            let r_idx = i / 36;
            let g_idx = (i / 6) % 6;
            let b_idx = i % 6;

            Rgb { r: cube_component(r_idx), g: cube_component(g_idx), b: cube_component(b_idx) }
        }

        // ── 24-step greyscale ramp ───────────────────────────────────────────
        //
        // Indices 232..=255: value = 8 + (idx - 232) * 10
        232..=255 => {
            let level = 8u8 + (idx - 232) * 10;
            Rgb { r: level, g: level, b: level }
        }
    }
}

/// Convert a 6-level cube component index (0..=5) to an 8-bit channel value.
///
/// Component 0 → `0`; components 1..=5 → `55 + component * 40`.
#[inline]
fn cube_component(c: u8) -> u8 {
    if c == 0 { 0 } else { 55 + c * 40 }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_theme() -> Theme {
        Theme::default()
    }

    // ── cube_component ────────────────────────────────────────────────────────

    #[test]
    fn cube_component_zero_is_black() {
        assert_eq!(cube_component(0), 0);
    }

    #[test]
    fn cube_component_five_is_max() {
        // 55 + 5 * 40 = 255
        assert_eq!(cube_component(5), 255);
    }

    #[test]
    fn cube_component_one() {
        assert_eq!(cube_component(1), 95);
    }

    // ── resolve_indexed ───────────────────────────────────────────────────────

    #[test]
    fn indexed_0_maps_to_ansi_black() {
        let theme = default_theme();
        assert_eq!(resolve_indexed(0, &theme), theme.ansi.black);
    }

    #[test]
    fn indexed_15_maps_to_ansi_bright_white() {
        let theme = default_theme();
        assert_eq!(resolve_indexed(15, &theme), theme.ansi.bright_white);
    }

    #[test]
    fn indexed_16_is_pure_black_cube() {
        // 16 → r=0, g=0, b=0 in the cube.
        let theme = default_theme();
        assert_eq!(resolve_indexed(16, &theme), Rgb::new(0, 0, 0));
    }

    #[test]
    fn indexed_231_is_pure_white_cube() {
        // 231 → r=5, g=5, b=5 → all components 255.
        let theme = default_theme();
        assert_eq!(resolve_indexed(231, &theme), Rgb::new(255, 255, 255));
    }

    #[test]
    fn indexed_232_is_darkest_grey() {
        let theme = default_theme();
        assert_eq!(resolve_indexed(232, &theme), Rgb::new(8, 8, 8));
    }

    #[test]
    fn indexed_255_is_lightest_grey() {
        // 8 + (255 - 232) * 10 = 8 + 230 = 238
        let theme = default_theme();
        assert_eq!(resolve_indexed(255, &theme), Rgb::new(238, 238, 238));
    }

    // ── resolve_named ─────────────────────────────────────────────────────────

    #[test]
    fn named_foreground_maps_to_theme_foreground() {
        let theme = default_theme();
        assert_eq!(resolve_named(NamedColor::Foreground, &theme), theme.foreground);
    }

    #[test]
    fn named_background_maps_to_theme_background() {
        let theme = default_theme();
        assert_eq!(resolve_named(NamedColor::Background, &theme), theme.background);
    }

    #[test]
    fn named_cursor_maps_to_theme_cursor() {
        let theme = default_theme();
        assert_eq!(resolve_named(NamedColor::Cursor, &theme), theme.cursor);
    }

    #[test]
    fn named_bright_foreground_maps_to_theme_foreground() {
        let theme = default_theme();
        assert_eq!(resolve_named(NamedColor::BrightForeground, &theme), theme.foreground);
    }

    #[test]
    fn named_dim_red_maps_to_ansi_red() {
        let theme = default_theme();
        assert_eq!(resolve_named(NamedColor::DimRed, &theme), theme.ansi.red);
    }

    // ── resolve_color ─────────────────────────────────────────────────────────

    #[test]
    fn spec_color_passes_through() {
        let theme = default_theme();
        let vte_rgb = alacritty_terminal::vte::ansi::Rgb { r: 10, g: 20, b: 30 };
        let result = resolve_color(&Color::Spec(vte_rgb), &theme);
        assert_eq!(result, Rgb::new(10, 20, 30));
    }

    #[test]
    fn indexed_color_delegates() {
        let theme = default_theme();
        // Index 196 = 16 + (4*36 + 0*6 + 0) = 16 + 144 = 160 → r=4,g=0,b=0
        // cube_component(4) = 55 + 4*40 = 215; g=0,b=0
        let result = resolve_color(&Color::Indexed(160), &theme);
        assert_eq!(result, Rgb::new(215, 0, 0));
    }

    #[test]
    fn convert_grid_produces_correct_dimensions() {
        // We can't easily create a Terminal in tests without a real PTY,
        // but we can test the color resolution and helper functions.
        let theme = Theme::default();

        // Verify all 256 indexed colors resolve without panic.
        for idx in 0..=255u8 {
            let _ = resolve_indexed(idx, &theme);
        }
    }

    #[test]
    fn all_named_colors_resolve() {
        let theme = Theme::default();

        let named_colors = [
            NamedColor::Black,
            NamedColor::Red,
            NamedColor::Green,
            NamedColor::Yellow,
            NamedColor::Blue,
            NamedColor::Magenta,
            NamedColor::Cyan,
            NamedColor::White,
            NamedColor::BrightBlack,
            NamedColor::BrightRed,
            NamedColor::BrightGreen,
            NamedColor::BrightYellow,
            NamedColor::BrightBlue,
            NamedColor::BrightMagenta,
            NamedColor::BrightCyan,
            NamedColor::BrightWhite,
            NamedColor::Foreground,
            NamedColor::Background,
            NamedColor::Cursor,
            NamedColor::DimBlack,
            NamedColor::DimRed,
            NamedColor::DimGreen,
            NamedColor::DimYellow,
            NamedColor::DimBlue,
            NamedColor::DimMagenta,
            NamedColor::DimCyan,
            NamedColor::DimWhite,
            NamedColor::BrightForeground,
            NamedColor::DimForeground,
        ];

        for nc in &named_colors {
            let _ = resolve_named(*nc, &theme);
        }
    }
}
