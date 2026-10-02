//! IME composition is a display overlay; only committed text reaches the PTY.

use mechanic_config::theme::Theme;
use mechanic_renderer::{CellFlags, CursorStyle, RenderCell, RenderGrid};
use unicode_width::UnicodeWidthChar;

pub struct Preedit {
    text: String,
    selection: Option<(usize, usize)>,
}

impl Preedit {
    pub fn new(text: String, selection: Option<(usize, usize)>) -> Option<Self> {
        if text.is_empty() {
            return None;
        }
        let boundary = |offset: usize| {
            let mut offset = offset.min(text.len());
            while !text.is_char_boundary(offset) {
                offset -= 1;
            }
            offset
        };
        let selection = selection.map(|(start, end)| {
            let (start, end) = (boundary(start), boundary(end));
            (start.min(end), start.max(end))
        });
        Some(Self { text, selection })
    }

    /// Clip to the current row rather than inventing terminal wraps or scrolling.
    pub fn overlay(&self, grid: &mut RenderGrid, theme: &Theme) {
        let (mut col, row) = grid.cursor_position;
        if col >= grid.cols || row >= grid.rows {
            return;
        }
        let mut previous = None;
        let mut caret = None;
        for (offset, character) in self.text.char_indices() {
            if matches!(character, '\n' | '\r') {
                break;
            }
            let Some(width) = character.width() else { continue };
            let selected = self
                .selection
                .is_some_and(|(start, end)| offset < end && offset + character.len_utf8() > start);
            if self.selection.is_some_and(|(start, _)| offset >= start) && caret.is_none() {
                caret = Some(if width == 0 { previous.unwrap_or(col) } else { col });
            }
            if width == 0
                && let Some(base) = previous
            {
                let cell = grid.get_mut(base, row).unwrap();
                cell.zerowidth.push(character);
                if selected {
                    cell.bg = theme.selection.background;
                    cell.fg = theme.selection.foreground.unwrap_or(theme.foreground);
                    if cell.flags.contains(CellFlags::WIDE_CHAR) {
                        let spacer = grid.get_mut(base + 1, row).unwrap();
                        spacer.bg = theme.selection.background;
                        spacer.fg = theme.selection.foreground.unwrap_or(theme.foreground);
                    }
                }
                continue;
            }
            // A leading combining mark needs a visible base of its own.
            let span = width.max(1);
            if col + span > grid.cols {
                break;
            }
            for covered in col..col + span {
                clear_wide_pair(grid, covered, row, theme);
            }
            let bg = if selected { theme.selection.background } else { theme.background };
            let fg = if selected {
                theme.selection.foreground.unwrap_or(theme.foreground)
            } else {
                theme.foreground
            };
            let mut flags = CellFlags::UNDERLINE;
            if span == 2 {
                flags |= CellFlags::WIDE_CHAR;
            }
            *grid.get_mut(col, row).unwrap() = RenderCell {
                character: if width == 0 { '◌' } else { character },
                zerowidth: if width == 0 { character.to_string() } else { String::new() },
                fg,
                bg,
                flags,
                ..RenderCell::default()
            };
            if span == 2 {
                *grid.get_mut(col + 1, row).unwrap() = RenderCell {
                    fg,
                    bg,
                    flags: CellFlags::WIDE_CHAR_SPACER | CellFlags::UNDERLINE,
                    ..RenderCell::default()
                };
            }
            previous = Some(col);
            col += span;
        }
        grid.cursor_position = (caret.unwrap_or(col).min(grid.cols - 1), row);
        grid.cursor_visible = self.selection.is_some();
        grid.cursor_width = 1;
        grid.cursor_style = CursorStyle::Bar;
        grid.cursor_color = theme.cursor;
    }
}

fn clear_wide_pair(grid: &mut RenderGrid, col: usize, row: usize, theme: &Theme) {
    let flags = grid.get(col, row).unwrap().flags;
    let neighbor = if flags.contains(CellFlags::WIDE_CHAR_SPACER) {
        col.checked_sub(1)
    } else if flags.contains(CellFlags::WIDE_CHAR) && col + 1 < grid.cols {
        Some(col + 1)
    } else {
        None
    };
    if let Some(neighbor) = neighbor {
        *grid.get_mut(neighbor, row).unwrap() =
            RenderCell { fg: theme.foreground, bg: theme.background, ..RenderCell::default() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlay(grid: &mut RenderGrid, text: &str, selection: Option<(usize, usize)>) {
        Preedit::new(text.into(), selection).unwrap().overlay(grid, &Theme::default());
    }

    #[test]
    fn composition_preserves_combining_marks_and_wide_cell_pairs() {
        let mut grid = RenderGrid::new(8, 2);
        grid.cursor_position = (1, 1);
        overlay(&mut grid, "e\u{301}界", Some((3, 6)));
        let base = grid.get(1, 1).unwrap();
        assert_eq!(base.character, 'e');
        assert_eq!(base.zerowidth, "\u{301}");
        assert!(base.flags.contains(CellFlags::UNDERLINE));
        let wide = grid.get(2, 1).unwrap();
        assert_eq!(wide.character, '界');
        assert!(wide.flags.contains(CellFlags::WIDE_CHAR));
        let spacer = grid.get(3, 1).unwrap();
        assert!(spacer.flags.contains(CellFlags::WIDE_CHAR_SPACER));
        assert_eq!(wide.bg, Theme::default().selection.background);
        assert_eq!(spacer.bg, wide.bg);
        assert_eq!(grid.cursor_position, (2, 1));
        assert_eq!(grid.cursor_style, CursorStyle::Bar);
        assert_eq!(grid.get(1, 0).unwrap().character, ' ');
    }

    #[test]
    fn selection_offsets_are_utf8_bytes_and_invalid_boundaries_are_safe() {
        let mut grid = RenderGrid::new(8, 1);
        overlay(&mut grid, "é界x", Some((4, 2)));
        // Reversed offsets are normalized; byte 4 is rounded to the start of 界.
        assert_eq!(grid.cursor_position, (1, 0));
        assert_eq!(grid.get(0, 0).unwrap().bg, Theme::default().background);
        let mut grid = RenderGrid::new(8, 1);
        overlay(&mut grid, "é界x", Some((2, usize::MAX)));
        assert_eq!(grid.get(1, 0).unwrap().bg, Theme::default().selection.background);
        assert_eq!(grid.get(3, 0).unwrap().bg, Theme::default().selection.background);
    }

    #[test]
    fn clipping_does_not_split_a_wide_character_or_wrap_into_another_row() {
        let mut grid = RenderGrid::new(4, 2);
        grid.cursor_position = (2, 0);
        grid.get_mut(0, 1).unwrap().character = 'z';
        overlay(&mut grid, "a界b", Some((5, 5)));
        assert_eq!(grid.get(2, 0).unwrap().character, 'a');
        assert_eq!(grid.get(3, 0).unwrap().character, ' ');
        assert_eq!(grid.get(0, 1).unwrap().character, 'z');
        assert_eq!(grid.cursor_position, (3, 0));
        assert!(!grid.wrapped[0]);
    }

    #[test]
    fn replacing_half_an_existing_wide_character_clears_its_other_half() {
        for anchor in [0, 1] {
            let mut grid = RenderGrid::new(4, 1);
            grid.get_mut(0, 0).unwrap().character = '界';
            grid.get_mut(0, 0).unwrap().flags = CellFlags::WIDE_CHAR;
            grid.get_mut(1, 0).unwrap().flags = CellFlags::WIDE_CHAR_SPACER;
            grid.cursor_position = (anchor, 0);
            overlay(&mut grid, "x", Some((0, 0)));
            assert_eq!(grid.get(anchor, 0).unwrap().character, 'x');
            let other = grid.get(1 - anchor, 0).unwrap();
            assert_eq!(other.character, ' ');
            assert!(other.flags.is_empty());
        }
    }

    #[test]
    fn leading_combining_marks_have_a_display_base_and_none_hides_caret() {
        let mut grid = RenderGrid::new(4, 1);
        overlay(&mut grid, "\u{301}\u{302}", None);
        assert_eq!(grid.get(0, 0).unwrap().character, '◌');
        assert_eq!(grid.get(0, 0).unwrap().zerowidth, "\u{301}\u{302}");
        assert!(!grid.cursor_visible);
    }

    #[test]
    fn selecting_combining_mark_recolors_both_halves_of_its_wide_base() {
        let mut theme = Theme::default();
        theme.selection.foreground = Some(mechanic_config::theme::Rgb::new(1, 2, 3));
        let mut grid = RenderGrid::new(4, 1);
        Preedit::new("界\u{301}".into(), Some((3, 5))).unwrap().overlay(&mut grid, &theme);
        for col in 0..2 {
            assert_eq!(grid.get(col, 0).unwrap().fg, theme.selection.foreground.unwrap());
            assert_eq!(grid.get(col, 0).unwrap().bg, theme.selection.background);
        }
    }

    #[test]
    fn empty_preedit_clears_and_offscreen_cursor_leaves_grid_unchanged() {
        assert!(Preedit::new(String::new(), Some((0, 0))).is_none());
        let mut grid = RenderGrid::new(4, 1);
        grid.cursor_position = (0, 3);
        let before = grid.cells.clone();
        overlay(&mut grid, "test", Some((0, 0)));
        assert_eq!(grid.cells, before);
        assert_eq!(grid.cursor_position, (0, 3));
    }
}
