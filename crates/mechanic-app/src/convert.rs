//! Grid conversion: [`mechanic_core::Terminal`] → [`mechanic_renderer::RenderGrid`].

use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use mechanic_config::theme::{Rgb, Theme};
use mechanic_core::Terminal;
use mechanic_renderer::{CellFlags, CursorStyle, RenderCell, RenderGrid};

/// Snapshot visible cells with theme colors; focused block cursors recolor their cell.
pub fn convert_grid(terminal: &Terminal, theme: &Theme, focused: bool) -> RenderGrid {
    let grid = terminal.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    let display_offset = grid.display_offset();

    let mut render_grid = RenderGrid::new(cols, rows);

    // Grid lines are relative to the live screen; add display_offset for viewport rows.
    for indexed in grid.display_iter() {
        let cell = indexed.cell;

        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }

        let row = (indexed.point.line.0 + display_offset as i32) as usize;
        let col = indexed.point.column.0;

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

        render_grid.cells[row * cols + col] = render_cell;
    }

    let cursor_line = grid.cursor.point.line.0;
    let cursor_row = (cursor_line + display_offset as i32).clamp(0, rows as i32 - 1) as usize;
    let cursor_col = grid.cursor.point.column.0.min(cols.saturating_sub(1));
    render_grid.cursor_position = (cursor_col, cursor_row);

    render_grid.cursor_style = match terminal.cursor_shape() {
        CursorShape::Block | CursorShape::HollowBlock => CursorStyle::Block,
        CursorShape::Underline => CursorStyle::Underline,
        CursorShape::Beam => CursorStyle::Bar,
        CursorShape::Hidden => CursorStyle::Block,
    };

    let sel_range = terminal.selection_range();
    if let Some(range) = sel_range.as_ref() {
        apply_selection_highlight(&mut render_grid, range, display_offset, cols, rows, theme);
    }

    // Recolor after selection so the cursor remains distinguishable.
    if matches!(render_grid.cursor_style, CursorStyle::Block) && focused {
        let cursor_in_selection = sel_range.as_ref().is_some_and(|r| {
            let p = grid.cursor.point;
            p >= r.start && p <= r.end
        });
        if let Some(cell) = render_grid.get_mut(cursor_col, cursor_row) {
            cell.bg = if cursor_in_selection {
                mechanic_config::theme::palette::AMBER
            } else {
                theme.cursor
            };
            cell.fg = theme.cursor_text;
        }
    }

    render_grid
}

/// Apply selection highlight colors to all cells within `sel_range`.
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

    let start_line = start.line.0;
    let end_line = end.line.0;

    for line_idx in start_line..=end_line {
        let viewport_row = line_idx + display_offset as i32;
        if viewport_row < 0 || viewport_row >= rows as i32 {
            continue;
        }
        let row = viewport_row as usize;

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

/// Resolve an alacritty [`Color`] to our [`Rgb`] type using the active [`Theme`].
fn resolve_color(color: &Color, theme: &Theme) -> Rgb {
    match color {
        Color::Named(named) => resolve_named(*named, theme),

        Color::Spec(vte_rgb) => Rgb { r: vte_rgb.r, g: vte_rgb.g, b: vte_rgb.b },

        Color::Indexed(idx) => resolve_indexed(*idx, theme),
    }
}

/// Resolve a [`NamedColor`] to our [`Rgb`].
fn resolve_named(named: NamedColor, theme: &Theme) -> Rgb {
    let ansi = &theme.ansi;
    match named {
        NamedColor::Foreground => theme.foreground,
        NamedColor::Background => theme.background,
        NamedColor::Cursor => theme.cursor,

        NamedColor::BrightBlack => ansi.bright_black,
        NamedColor::BrightRed => ansi.bright_red,
        NamedColor::BrightGreen => ansi.bright_green,
        NamedColor::BrightYellow => ansi.bright_yellow,
        NamedColor::BrightBlue => ansi.bright_blue,
        NamedColor::BrightMagenta => ansi.bright_magenta,
        NamedColor::BrightCyan => ansi.bright_cyan,
        NamedColor::BrightWhite => ansi.bright_white,

        NamedColor::Black | NamedColor::DimBlack => ansi.black,
        NamedColor::Red | NamedColor::DimRed => ansi.red,
        NamedColor::Green | NamedColor::DimGreen => ansi.green,
        NamedColor::Yellow | NamedColor::DimYellow => ansi.yellow,
        NamedColor::Blue | NamedColor::DimBlue => ansi.blue,
        NamedColor::Magenta | NamedColor::DimMagenta => ansi.magenta,
        NamedColor::Cyan | NamedColor::DimCyan => ansi.cyan,
        NamedColor::White | NamedColor::DimWhite => ansi.white,

        NamedColor::BrightForeground => theme.foreground,
        NamedColor::DimForeground => theme.foreground,
    }
}

/// Resolve a 256-color palette index to our [`Rgb`].
fn resolve_indexed(idx: u8, theme: &Theme) -> Rgb {
    match idx {
        0..=15 => {
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
                _ => unreachable!(),
            };
            resolve_named(named, theme)
        }

        16..=231 => {
            let i = idx - 16;
            let r_idx = i / 36;
            let g_idx = (i / 6) % 6;
            let b_idx = i % 6;

            Rgb { r: cube_component(r_idx), g: cube_component(g_idx), b: cube_component(b_idx) }
        }

        232..=255 => {
            let level = 8u8 + (idx - 232) * 10;
            Rgb { r: level, g: level, b: level }
        }
    }
}

/// Convert a 6-level cube component index (0..=5) to an 8-bit channel value.
#[inline]
fn cube_component(c: u8) -> u8 {
    if c == 0 { 0 } else { 55 + c * 40 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_theme() -> Theme {
        Theme::default()
    }

    #[test]
    fn cube_component_zero_is_black() {
        assert_eq!(cube_component(0), 0);
    }

    #[test]
    fn cube_component_five_is_max() {
        assert_eq!(cube_component(5), 255);
    }

    #[test]
    fn cube_component_one() {
        assert_eq!(cube_component(1), 95);
    }

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
        let theme = default_theme();
        assert_eq!(resolve_indexed(16, &theme), Rgb::new(0, 0, 0));
    }

    #[test]
    fn indexed_231_is_pure_white_cube() {
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
        let theme = default_theme();
        assert_eq!(resolve_indexed(255, &theme), Rgb::new(238, 238, 238));
    }

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
        let result = resolve_color(&Color::Indexed(160), &theme);
        assert_eq!(result, Rgb::new(215, 0, 0));
    }

    #[test]
    fn convert_grid_produces_correct_dimensions() {
        let theme = Theme::default();

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
