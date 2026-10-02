//! Grid conversion: [`mechanic_core::Terminal`] → [`mechanic_renderer::RenderGrid`].

use alacritty_terminal::grid::{Dimensions as _, Grid};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::RenderableContent;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use mechanic_config::theme::{Rgb, Theme};
use mechanic_core::Terminal;
use mechanic_renderer::{CellFlags, CursorStyle, RenderCell, RenderGrid};

/// Snapshot visible cells with theme colors; focused block cursors recolor their cell.
pub fn convert_grid(terminal: &Terminal, theme: &Theme, focused: bool) -> RenderGrid {
    let grid = terminal.grid();
    let mut render_grid = convert_content(
        terminal.renderable_content(),
        grid.columns(),
        grid.screen_lines(),
        theme,
        focused,
    );
    append_bidi_context(&mut render_grid, grid);
    render_grid
}

const BIDI_CONTEXT_LIMIT: usize = 64 * 1024;

fn context_cell_len(cell: &Cell, remaining: usize) -> Option<usize> {
    let mut bytes = cell.c.len_utf8();
    if bytes > remaining {
        return None;
    }
    for character in cell.zerowidth().unwrap_or_default() {
        bytes += character.len_utf8();
        if bytes > remaining {
            return None;
        }
    }
    Some(bytes)
}

fn append_bidi_context(render_grid: &mut RenderGrid, grid: &Grid<Cell>) {
    if render_grid.cols == 0 || render_grid.rows == 0 {
        return;
    }
    let last_col = Column(render_grid.cols - 1);
    let first_visible = -(grid.display_offset() as i32);
    let spacer_flags = Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER;

    // Scan backwards, retaining the nearest preceding complete cells. Reverse
    // once at the end so their combining sequences remain in logical order.
    let mut prefix = Vec::new();
    let mut prefix_bytes = 0;
    let mut scanned = 0;
    let mut line = first_visible - 1;
    'prefix: while line >= grid.topmost_line().0
        && grid[Line(line)][last_col].flags.contains(Flags::WRAPLINE)
    {
        for col in (0..render_grid.cols).rev() {
            scanned += 1;
            if scanned > BIDI_CONTEXT_LIMIT {
                render_grid.bidi_context_truncated = true;
                break 'prefix;
            }
            let cell = &grid[Line(line)][Column(col)];
            if cell.flags.intersects(spacer_flags) {
                continue;
            }
            let Some(bytes) = context_cell_len(cell, BIDI_CONTEXT_LIMIT - prefix_bytes) else {
                render_grid.bidi_context_truncated = true;
                break 'prefix;
            };
            prefix_bytes += bytes;
            prefix.extend(cell.zerowidth().unwrap_or_default().iter().rev().copied());
            prefix.push(cell.c);
        }
        line -= 1;
    }
    render_grid.bidi_prefix = prefix.into_iter().rev().collect();

    let mut suffix = String::new();
    scanned = 0;
    line = first_visible + render_grid.rows as i32 - 1;
    'suffix: while line < grid.bottommost_line().0
        && grid[Line(line)][last_col].flags.contains(Flags::WRAPLINE)
    {
        line += 1;
        for col in 0..render_grid.cols {
            scanned += 1;
            if scanned > BIDI_CONTEXT_LIMIT {
                render_grid.bidi_context_truncated = true;
                break 'suffix;
            }
            let cell = &grid[Line(line)][Column(col)];
            if cell.flags.intersects(spacer_flags) {
                continue;
            }
            if context_cell_len(cell, BIDI_CONTEXT_LIMIT - suffix.len()).is_none() {
                render_grid.bidi_context_truncated = true;
                break 'suffix;
            }
            suffix.push(cell.c);
            suffix.extend(cell.zerowidth().unwrap_or_default().iter());
        }
    }
    render_grid.bidi_suffix = suffix;
}

fn convert_content(
    content: RenderableContent<'_>,
    cols: usize,
    rows: usize,
    theme: &Theme,
    focused: bool,
) -> RenderGrid {
    let RenderableContent { display_iter, selection, cursor, display_offset, colors, .. } = content;
    let color = |value: &Color| resolve_with_overrides(value, colors, theme);

    let mut render_grid = RenderGrid::new(cols, rows);

    // Grid lines are relative to the live screen; add display_offset for viewport rows.
    for indexed in display_iter {
        let cell = indexed.cell;
        let Ok(row) = usize::try_from(indexed.point.line.0 + display_offset as i32) else {
            continue;
        };
        let col = indexed.point.column.0;

        if row >= rows || col >= cols {
            continue;
        }
        if col + 1 == cols {
            render_grid.wrapped[row] = cell.flags.contains(Flags::WRAPLINE);
        }

        let mut flags = CellFlags::empty();
        if cell.flags.contains(Flags::BOLD) {
            flags |= CellFlags::BOLD;
        }
        if cell.flags.contains(Flags::ITALIC) {
            flags |= CellFlags::ITALIC;
        }
        if cell.flags.intersects(Flags::ALL_UNDERLINES) || cell.hyperlink().is_some() {
            flags |= CellFlags::UNDERLINE;
        }
        for (source, target) in [
            (Flags::WIDE_CHAR, CellFlags::WIDE_CHAR),
            (Flags::WIDE_CHAR_SPACER, CellFlags::WIDE_CHAR_SPACER),
            (Flags::LEADING_WIDE_CHAR_SPACER, CellFlags::LEADING_WIDE_CHAR_SPACER),
            (Flags::HIDDEN, CellFlags::HIDDEN),
            (Flags::DOUBLE_UNDERLINE, CellFlags::DOUBLE_UNDERLINE),
            (Flags::UNDERCURL, CellFlags::UNDERCURL),
            (Flags::DOTTED_UNDERLINE, CellFlags::DOTTED_UNDERLINE),
            (Flags::DASHED_UNDERLINE, CellFlags::DASHED_UNDERLINE),
            (Flags::STRIKEOUT, CellFlags::STRIKEOUT),
        ] {
            if cell.flags.contains(source) {
                flags |= target;
            }
        }

        let mut fg = color(&cell.fg);
        let mut bg = color(&cell.bg);
        // Inversion belongs to source text colors. Selection and cursor colors
        // are overlays and must not be inverted again by the renderer.
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }

        let render_cell = RenderCell {
            character: cell.c,
            zerowidth: cell.zerowidth().map(|chars| chars.iter().collect()).unwrap_or_default(),
            fg,
            bg,
            flags,
            underline_color: cell.underline_color().map(|value| color(&value)),
        };

        render_grid.cells[row * cols + col] = render_cell;
    }

    let cursor_row =
        usize::try_from(cursor.point.line.0 + display_offset as i32).unwrap_or(usize::MAX);
    let cursor_col = cursor.point.column.0;
    render_grid.cursor_position = (cursor_col, cursor_row);
    render_grid.cursor_visible =
        cursor.shape != CursorShape::Hidden && cursor_col < cols && cursor_row < rows;
    render_grid.cursor_color = color(&Color::Named(NamedColor::Cursor));
    if render_grid
        .get(cursor_col, cursor_row)
        .is_some_and(|cell| cell.flags.contains(CellFlags::WIDE_CHAR))
    {
        render_grid.cursor_width = 2.min(cols - cursor_col);
    }

    render_grid.cursor_style = match (focused, cursor.shape) {
        (false, _) | (_, CursorShape::HollowBlock) => CursorStyle::HollowBlock,
        (_, CursorShape::Block | CursorShape::Hidden) => CursorStyle::Block,
        (_, CursorShape::Underline) => CursorStyle::Underline,
        (_, CursorShape::Beam) => CursorStyle::Bar,
    };

    if let Some(range) = selection.as_ref() {
        apply_selection_highlight(&mut render_grid, range, display_offset, cols, rows, theme);
    }

    // Recolor after selection so the cursor remains distinguishable.
    if render_grid.cursor_visible && render_grid.cursor_style == CursorStyle::Block {
        let cursor_in_selection = selection.as_ref().is_some_and(|range| {
            (0..render_grid.cursor_width).any(|offset| {
                range.contains(Point::new(cursor.point.line, Column(cursor_col + offset)))
            })
        });
        let cursor_bg = if cursor_in_selection {
            mechanic_config::theme::palette::AMBER
        } else {
            render_grid.cursor_color
        };
        render_grid.cursor_color = cursor_bg;
        for offset in 0..render_grid.cursor_width {
            if let Some(cell) = render_grid.get_mut(cursor_col + offset, cursor_row) {
                cell.bg = cursor_bg;
                cell.fg = theme.cursor_text;
            }
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

    // Bound work to visible cells even when a selection spans all scrollback.
    for row in 0..rows {
        let line = Line(row as i32 - display_offset as i32);
        for col in 0..cols {
            let cell = &mut render_grid.cells[row * cols + col];
            let selected = sel_range.contains(Point::new(line, Column(col)))
                || (cell.flags.contains(CellFlags::WIDE_CHAR)
                    && col + 1 < cols
                    && sel_range.contains(Point::new(line, Column(col + 1))))
                || (cell.flags.contains(CellFlags::WIDE_CHAR_SPACER)
                    && col > 0
                    && sel_range.contains(Point::new(line, Column(col - 1))));
            if selected {
                cell.bg = sel_bg;
                if let Some(fg) = sel_fg {
                    cell.fg = fg;
                }
            }
        }
    }
}

/// Resolve an alacritty [`Color`] to our [`Rgb`] type using the active [`Theme`].
fn resolve_with_overrides(
    color: &Color,
    colors: &alacritty_terminal::term::color::Colors,
    theme: &Theme,
) -> Rgb {
    let overridden = match color {
        Color::Named(index) => colors[*index],
        Color::Indexed(index) => colors[*index as usize],
        Color::Spec(_) => None,
    };
    overridden.map_or_else(|| resolve_color(color, theme), |rgb| Rgb::new(rgb.r, rgb.g, rgb.b))
}

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
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Scroll;
    use alacritty_terminal::index::Side;
    use alacritty_terminal::selection::{Selection, SelectionType};
    use alacritty_terminal::term::Term;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::vte::ansi::Processor;

    #[test]
    fn osc8_labels_are_underlined_without_changing_sgr_styles_or_following_text() {
        let term = parsed_term(
            10,
            2,
            "\x1b]8;;https://example.com\x1b\\A\x1b[4:3mB\x1b]8;;\x1b\\\x1b[0mC",
        );
        let grid = snapshot(&term, &Theme::default(), false);
        assert!(grid.cells[0].flags.contains(CellFlags::UNDERLINE));
        assert!(grid.cells[1].flags.contains(CellFlags::UNDERCURL));
        assert!(!grid.cells[2].flags.contains(CellFlags::UNDERLINE));
    }

    #[test]
    fn osc_palette_changes_and_resets_reach_cells_and_cursor() {
        let mut term = parsed_term(
            8,
            2,
            "\x1b]4;1;#123456\x07\x1b]10;#abcdef\x07\x1b]11;#112233\x07\x1b]12;#445566\x07\x1b[31mR\x1b[39mF",
        );
        let theme = Theme::default();
        let grid = snapshot(&term, &theme, false);
        assert_eq!(grid.cells[0].fg, Rgb::from_hex(0x123456));
        assert_eq!(grid.cells[1].fg, Rgb::from_hex(0xabcdef));
        assert_eq!(grid.cells[0].bg, Rgb::from_hex(0x112233));
        assert_eq!(grid.cursor_color, Rgb::from_hex(0x445566));
        Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new()
            .advance(&mut term, b"\x1b]104;1\x07\x1b]110\x07\x1b]111\x07\x1b]112\x07");
        let grid = snapshot(&term, &theme, false);
        assert_eq!(grid.cells[0].fg, theme.ansi.red);
        assert_eq!(grid.cells[1].fg, theme.foreground);
        assert_eq!(grid.cells[0].bg, theme.background);
        assert_eq!(grid.cursor_color, theme.cursor);
    }

    #[test]
    fn sgr_decoration_styles_and_color_are_preserved() {
        let term = parsed_term(
            10,
            2,
            "\x1b[4:1mA\x1b[4:2mB\x1b[4:3mC\x1b[4:4mD\x1b[4:5mE\x1b[9;58:2::1:2:3mF\x1b[0mG",
        );
        let grid = snapshot(&term, &Theme::default(), false);
        for (col, flag) in [
            CellFlags::UNDERLINE,
            CellFlags::DOUBLE_UNDERLINE,
            CellFlags::UNDERCURL,
            CellFlags::DOTTED_UNDERLINE,
            CellFlags::DASHED_UNDERLINE,
            CellFlags::STRIKEOUT,
        ]
        .into_iter()
        .enumerate()
        {
            assert!(grid.cells[col].flags.contains(flag), "column {col}");
        }
        assert_eq!(grid.cells[5].underline_color, Some(Rgb::new(1, 2, 3)));
        assert!(!grid.cells[6].flags.intersects(CellFlags::UNDERLINE | CellFlags::STRIKEOUT));
    }

    fn parsed_term(cols: usize, rows: usize, source: &str) -> Term<VoidListener> {
        let mut term = Term::new(Default::default(), &TermSize::new(cols, rows), VoidListener);
        let mut processor: Processor = Processor::new();
        processor.advance(&mut term, source.as_bytes());
        term
    }

    fn snapshot(term: &Term<VoidListener>, theme: &Theme, focused: bool) -> RenderGrid {
        let mut grid = convert_content(
            term.renderable_content(),
            term.columns(),
            term.screen_lines(),
            theme,
            focused,
        );
        append_bidi_context(&mut grid, term.grid());
        grid
    }

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
        let grid = snapshot(&parsed_term(7, 3, "hello"), &theme, true);
        assert_eq!((grid.cols, grid.rows, grid.cells.len()), (7, 3, 21));
        assert_eq!(grid.get(0, 0).unwrap().character, 'h');
    }

    #[test]
    fn combining_characters_keep_source_order() {
        let grid = snapshot(&parsed_term(8, 2, "e\u{301}\u{308}"), &Theme::default(), true);
        let cell = grid.get(0, 0).unwrap();
        assert_eq!(cell.character, 'e');
        assert_eq!(cell.zerowidth, "\u{301}\u{308}");
        assert!(grid.get(1, 0).unwrap().zerowidth.is_empty());
    }

    #[test]
    fn soft_wrap_metadata_distinguishes_hard_newlines() {
        let grid = snapshot(&parsed_term(4, 3, "abcde\r\nxyz"), &Theme::default(), true);
        assert_eq!(grid.wrapped, vec![true, false, false]);
        assert_eq!(grid.get(0, 1).unwrap().character, 'e');
        assert_eq!(grid.get(0, 2).unwrap().character, 'x');
    }

    #[test]
    fn viewport_middle_of_wrapped_paragraph_retains_context_on_both_sides() {
        let mut term = parsed_term(4, 2, "abcdefghijklmnopqrstuvwx");
        term.scroll_display(Scroll::Delta(2));
        let grid = snapshot(&term, &Theme::default(), true);
        assert_eq!(grid.get(0, 0).unwrap().character, 'i');
        assert_eq!(grid.get(0, 1).unwrap().character, 'm');
        assert_eq!(grid.bidi_prefix, "abcdefgh");
        assert_eq!(grid.bidi_suffix, "qrstuvwx");
        assert!(!grid.bidi_context_truncated);
    }

    #[test]
    fn bidi_context_retains_combining_order_and_skips_wide_padding() {
        let mut term = parsed_term(4, 2, "e\u{301}界xabc界yz");
        let grid = snapshot(&term, &Theme::default(), true);
        assert_eq!(grid.bidi_prefix, "e\u{301}界x");
        term.scroll_display(Scroll::Delta(1));
        let grid = snapshot(&term, &Theme::default(), true);
        assert_eq!(grid.bidi_suffix, "界yz");
    }

    #[test]
    fn hard_newlines_stop_viewport_bidi_context() {
        let mut term = parsed_term(4, 2, "abc\r\ndef\r\nghi");
        let grid = snapshot(&term, &Theme::default(), true);
        assert!(grid.bidi_prefix.is_empty());
        term.scroll_display(Scroll::Delta(1));
        let grid = snapshot(&term, &Theme::default(), true);
        assert!(grid.bidi_suffix.is_empty());
    }

    #[test]
    fn bidi_context_is_bounded_and_reports_truncation() {
        let term = parsed_term(64, 2, &"a".repeat(64 * 1102));
        let grid = snapshot(&term, &Theme::default(), true);
        assert_eq!(grid.bidi_prefix.len(), BIDI_CONTEXT_LIMIT);
        assert!(grid.bidi_context_truncated);
        let term = parsed_term(64, 2, &"界\u{301}".repeat(32 * 1102));
        let grid = snapshot(&term, &Theme::default(), true);
        assert!(grid.bidi_prefix.len() <= BIDI_CONTEXT_LIMIT);
        assert!(grid.bidi_context_truncated);
        assert!(grid.bidi_prefix.starts_with('界'));
        assert!(grid.bidi_prefix.ends_with("界\u{301}"));
    }

    #[test]
    fn german_copy_preserves_sharp_s_umlauts_and_quotes_after_reflow() {
        let original = "ÄÖÜäöüßẞ „Fünf große Füße grüßen Köln.“\nStraße STRASSE STRAẞE: Maße sind keine Masse. Gru\u{308}ße und A\u{308}O\u{308}U\u{308} a\u{308}o\u{308}u\u{308}; 10\u{a0}€ kosten die Bücher.";
        for initial_cols in [5, 8, 17, 43] {
            let mut term = parsed_term(initial_cols, 4, &original.replace('\n', "\r\n"));
            for cols in [initial_cols, 3, 29, 7, 43] {
                term.resize(TermSize::new(cols, 4));
                let start = Point::new(term.grid().topmost_line(), Column(0));
                let mut end = term.grid().cursor.point;
                if !term.grid().cursor.input_needs_wrap {
                    if end.column.0 == 0 {
                        end = Point::new(end.line - 1, term.grid().last_column());
                    } else {
                        end.column -= 1;
                    }
                }
                let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
                selection.update(end, Side::Right);
                term.selection = Some(selection);
                assert_eq!(
                    term.selection_to_string().as_deref(),
                    Some(original),
                    "width {initial_cols} → {cols}"
                );
                for offset in [0, 2, 7] {
                    term.scroll_display(Scroll::Delta(offset));
                    let grid = snapshot(&term, &Theme::default(), true);
                    assert_eq!(grid.cols, cols);
                    assert_eq!(term.selection_to_string().as_deref(), Some(original));
                }
            }
        }
    }

    #[test]
    fn wrapped_multilingual_article_copy_preserves_original_logical_order() {
        // Original newspaper-style prose, not a quotation from a published article.
        let original = "أعلنت الجهاتُ المعنيّة، صباحَ اليوم، إطلاقَ مشروعٍ جديدٍ للنقل في المدينة. تبدأ المرحلة الأولى في ١٥ أكتوبر ٢٠٢٦ وتشمل 24 محطةً. وقال التقرير: «لا تكتملُ التنميةُ إلا بمشاركة المجتمع»، مع تخصيص ٣٫٥ ملايين دولار. وتُنشر النتائج عبر Open Data؛ ويمكن متابعة الأخبار والأسئلة: هل يتحسّن العمل؟ نعم، بإذن الله.\nРусский: Новости дня; Українська: Ґрунт і єдність. 日本語: 東京の新聞。 Cafe\u{301} nin\u{303}o ac\u{327}a\u{303}o.";
        let mut term = parsed_term(17, 8, &original.replace('\n', "\r\n"));
        assert!(term.grid().topmost_line() < Line(0));
        let start = Point::new(term.grid().topmost_line(), Column(0));
        let mut end = term.grid().cursor.point;
        if !term.grid().cursor.input_needs_wrap {
            end.column -= 1;
        }
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        term.selection = Some(selection);
        assert_eq!(term.selection_to_string().as_deref(), Some(original));

        for (scroll, focused) in [(0, true), (3, false), (6, true)] {
            term.scroll_display(Scroll::Delta(scroll));
            let converted = snapshot(&term, &Theme::default(), focused);
            assert!(converted.wrapped.iter().any(|wrapped| *wrapped));
            // Display colors, cursor geometry and bidi context must leave the
            // logical source used for clipboard selection untouched.
            assert_eq!(term.selection_to_string().as_deref(), Some(original));
        }
        assert_eq!(original.matches('\n').count(), 1);
        assert_eq!(term.selection_to_string().unwrap().matches('\n').count(), 1);
    }

    #[test]
    fn wide_spacer_retains_background_and_hidden_text_retains_flags() {
        let theme = Theme::default();
        let grid =
            snapshot(&parsed_term(8, 2, "\x1b[48;2;10;20;30m界\x1b[8mx\x1b[?25l"), &theme, true);
        assert!(grid.get(0, 0).unwrap().flags.contains(CellFlags::WIDE_CHAR));
        assert!(grid.get(1, 0).unwrap().flags.contains(CellFlags::WIDE_CHAR_SPACER));
        assert_eq!(grid.get(0, 0).unwrap().bg, Rgb::new(10, 20, 30));
        assert_eq!(grid.get(1, 0).unwrap().bg, Rgb::new(10, 20, 30));
        assert_eq!(grid.get(2, 0).unwrap().character, 'x');
        assert!(grid.get(2, 0).unwrap().flags.contains(CellFlags::HIDDEN));
        assert!(!grid.cursor_visible);
    }

    #[test]
    fn wrapped_wide_padding_is_preserved() {
        let grid = snapshot(&parsed_term(4, 2, "abc界\x1b[?25l"), &Theme::default(), true);
        assert!(grid.get(3, 0).unwrap().flags.contains(CellFlags::LEADING_WIDE_CHAR_SPACER));
        assert!(grid.get(0, 1).unwrap().flags.contains(CellFlags::WIDE_CHAR));
        assert!(grid.get(1, 1).unwrap().flags.contains(CellFlags::WIDE_CHAR_SPACER));
    }

    #[test]
    fn hidden_cursor_mode_and_shape_do_not_recolor_cells() {
        let theme = Theme::default();
        let term = parsed_term(8, 2, "\x1b[?25l");
        let grid = snapshot(&term, &theme, true);
        assert!(!grid.cursor_visible);
        assert_eq!(grid.get(0, 0).unwrap().bg, theme.background);

        let term = parsed_term(8, 2, "");
        let mut content = term.renderable_content();
        content.cursor.shape = CursorShape::Hidden;
        let grid = convert_content(content, 8, 2, &theme, true);
        assert!(!grid.cursor_visible);
        assert_eq!(grid.get(0, 0).unwrap().bg, theme.background);
    }

    #[test]
    fn scrollback_does_not_clamp_live_cursor_into_view() {
        let theme = Theme::default();
        let mut term = parsed_term(8, 3, "a\r\nb\r\nc\r\nd\r\ne");
        term.scroll_display(Scroll::Delta(2));
        let grid = snapshot(&term, &theme, true);
        assert!(!grid.cursor_visible);
        assert!(grid.cursor_position.1 >= grid.rows);
        assert_eq!(grid.get(1, 2).unwrap().bg, theme.background);
    }

    #[test]
    fn focused_cursor_shapes_and_unfocused_hollow_are_preserved() {
        let theme = Theme::default();
        for (sequence, style) in [
            ("\x1b[2 q", CursorStyle::Block),
            ("\x1b[4 q", CursorStyle::Underline),
            ("\x1b[6 q", CursorStyle::Bar),
        ] {
            let term = parsed_term(8, 2, sequence);
            let focused = snapshot(&term, &theme, true);
            assert!(focused.cursor_visible);
            assert_eq!(focused.cursor_style, style);
            let unfocused = snapshot(&term, &theme, false);
            assert_eq!(unfocused.cursor_style, CursorStyle::HollowBlock);
            assert_eq!(unfocused.get(0, 0).unwrap().bg, theme.background);
        }
        let term = parsed_term(8, 2, "");
        let mut content = term.renderable_content();
        content.cursor.shape = CursorShape::HollowBlock;
        assert_eq!(
            convert_content(content, 8, 2, &theme, true).cursor_style,
            CursorStyle::HollowBlock
        );
    }

    #[test]
    fn wide_cursor_snaps_to_leading_cell_and_overrides_inverse_on_both_columns() {
        let theme = Theme {
            cursor: Rgb::new(1, 2, 3),
            cursor_text: Rgb::new(4, 5, 6),
            ..Default::default()
        };
        let term = parsed_term(8, 2, "\x1b[7m界\x1b[2G");
        let focused = snapshot(&term, &theme, true);
        assert_eq!(focused.cursor_position, (0, 0));
        assert_eq!(focused.cursor_width, 2);
        assert_eq!(focused.cursor_color, theme.cursor);
        for col in 0..2 {
            let cell = focused.get(col, 0).unwrap();
            assert_eq!(cell.bg, theme.cursor);
            assert_eq!(cell.fg, theme.cursor_text);
            assert!(!cell.flags.contains(CellFlags::INVERSE));
        }
        let unfocused = snapshot(&term, &theme, false);
        assert_eq!(unfocused.cursor_style, CursorStyle::HollowBlock);
        assert_eq!(unfocused.cursor_width, 2);
        assert_eq!(unfocused.get(0, 0).unwrap().bg, theme.foreground);
        assert_eq!(unfocused.get(0, 0).unwrap().fg, theme.background);
    }

    #[test]
    fn inverse_precedes_selection_and_block_selection_stays_rectangular() {
        let theme = Theme {
            selection: mechanic_config::SelectionColors {
                foreground: None,
                background: Rgb::new(1, 2, 3),
            },
            ..Default::default()
        };
        let term = parsed_term(8, 3, "\x1b[7mabc\r\ndef\r\nghi\x1b[?25l");
        let mut content = term.renderable_content();
        content.selection = Some(SelectionRange::new(
            Point::new(Line(0), Column(1)),
            Point::new(Line(2), Column(1)),
            true,
        ));
        let grid = convert_content(content, 8, 3, &theme, true);
        for row in 0..3 {
            assert_eq!(grid.get(1, row).unwrap().bg, theme.selection.background);
            assert_eq!(grid.get(1, row).unwrap().fg, theme.background);
            for col in [0, 2] {
                assert_eq!(grid.get(col, row).unwrap().bg, theme.foreground);
            }
        }
    }

    #[test]
    fn selection_on_wide_spacer_highlights_both_columns_before_cursor_overlay() {
        let theme = Theme::default();
        let term = parsed_term(8, 2, "界\x1b[1G");
        let mut content = term.renderable_content();
        content.selection = Some(SelectionRange::new(
            Point::new(Line(0), Column(1)),
            Point::new(Line(0), Column(1)),
            false,
        ));
        let focused = convert_content(content, 8, 2, &theme, true);
        for col in 0..2 {
            assert_eq!(focused.get(col, 0).unwrap().bg, mechanic_config::theme::palette::AMBER);
            assert_eq!(focused.get(col, 0).unwrap().fg, theme.cursor_text);
        }
        let mut content = term.renderable_content();
        content.selection = Some(SelectionRange::new(
            Point::new(Line(0), Column(1)),
            Point::new(Line(0), Column(1)),
            false,
        ));
        let unfocused = convert_content(content, 8, 2, &theme, false);
        for col in 0..2 {
            assert_eq!(unfocused.get(col, 0).unwrap().bg, theme.selection.background);
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

#[cfg(all(test, target_os = "macos"))]
#[path = "convert_bench.rs"]
mod convert_bench;
