use mechanic_config::{Rgb, Theme};
use mechanic_core::search::{SearchMatch, SearchOptions, SearchResults, search_grid};
use mechanic_core::{GridColumn, GridLine, GridPoint, Terminal};
use mechanic_renderer::RenderGrid;

#[derive(Default)]
pub struct Search {
    pub active: bool,
    query: String,
    case_sensitive: bool,
    results: SearchResults,
    current: Option<usize>,
    stale: bool,
}

impl Search {
    pub fn set_query(&mut self, query: String, terminal: &Terminal) {
        self.query = query;
        self.refresh(terminal);
    }

    pub fn refresh(&mut self, terminal: &Terminal) {
        self.results = search_grid(
            terminal.grid(),
            &self.query,
            SearchOptions { case_sensitive: self.case_sensitive, ..Default::default() },
        );
        let top =
            GridPoint::new(GridLine(-(terminal.grid().display_offset() as i32)), GridColumn(0));
        self.current = initial_match(&self.results.matches, top);
        self.stale = false;
    }

    pub fn set_case_sensitive(&mut self, enabled: bool, terminal: &Terminal) {
        self.case_sensitive = enabled;
        self.refresh(terminal);
    }

    pub fn invalidate(&mut self) -> bool {
        if !self.query.is_empty() && !self.stale {
            self.stale = true;
            self.results.matches.clear();
            self.current = None;
            return true;
        }
        false
    }

    pub fn navigate(&mut self, terminal: &Terminal, backwards: bool) {
        if self.stale {
            self.refresh(terminal);
        } else {
            self.current = next_match(self.current, self.results.matches.len(), backwards);
        }
    }

    pub fn current(&self) -> Option<SearchMatch> {
        self.current.and_then(|index| self.results.matches.get(index)).copied()
    }

    pub fn status(&self) -> String {
        if self.query.is_empty() {
            if self.case_sensitive {
                "Find in scrollback · exact text".into()
            } else {
                "Find in scrollback · ignore case".into()
            }
        } else if self.stale {
            "Output changed · Return to refresh".into()
        } else if self.results.query_too_long {
            "Query too long (maximum 4096 characters)".into()
        } else {
            let limit =
                if self.results.truncated { " · partial results (search limit)" } else { "" };
            format!(
                "{} of {} matches{limit}",
                self.current.map_or(0, |i| i + 1),
                self.results.matches.len()
            )
        }
    }

    pub fn highlight(&self, grid: &mut RenderGrid, terminal: &Terminal, theme: &Theme) {
        if !self.active || self.stale || self.results.matches.is_empty() {
            return;
        }
        let offset = terminal.grid().display_offset() as i32;
        let selection = terminal.selection_range();
        let current = self.current();
        let mut index = 0;
        let blend = |a: u8, b: u8| ((u16::from(a) * 2 + u16::from(b)) / 3) as u8;
        for row in 0..grid.rows {
            for col in 0..grid.cols {
                let point = GridPoint::new(GridLine(row as i32 - offset), GridColumn(col));
                while index < self.results.matches.len() && self.results.matches[index].end < point
                {
                    index += 1;
                }
                if index == self.results.matches.len() {
                    return;
                }
                let at_cursor = grid.cursor_visible
                    && row == grid.cursor_position.1
                    && col >= grid.cursor_position.0
                    && col < grid.cursor_position.0 + grid.cursor_width;
                if at_cursor
                    || selection.as_ref().is_some_and(|s| s.contains(point))
                    || !self.results.matches[index].contains(point)
                {
                    continue;
                }
                let cell = grid.get_mut(col, row).unwrap();
                if current.is_some_and(|hit| hit.contains(point)) {
                    cell.bg = theme.selection.background;
                    cell.fg = theme.selection.foreground.unwrap_or(theme.foreground);
                } else {
                    cell.bg = Rgb::new(
                        blend(cell.bg.r, theme.selection.background.r),
                        blend(cell.bg.g, theme.selection.background.g),
                        blend(cell.bg.b, theme.selection.background.b),
                    );
                }
            }
        }
    }
}

fn initial_match(matches: &[SearchMatch], top: GridPoint) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    Some(matches.iter().position(|hit| hit.end >= top).unwrap_or(matches.len() - 1))
}

fn next_match(current: Option<usize>, count: usize, backwards: bool) -> Option<usize> {
    if count == 0 {
        return None;
    }
    Some(match current {
        Some(index) if backwards => (index + count - 1) % count,
        Some(index) => (index + 1) % count,
        None if backwards => count - 1,
        None => 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_wraps_and_empty_results_are_safe() {
        assert_eq!(next_match(None, 0, false), None);
        assert_eq!(next_match(Some(0), 3, true), Some(2));
        assert_eq!(next_match(Some(2), 3, false), Some(0));
        assert_eq!(next_match(Some(0), 1, true), Some(0));
    }

    #[test]
    fn initial_result_can_span_viewport_top() {
        let point = |line| GridPoint::new(GridLine(line), GridColumn(0));
        let hits = [
            SearchMatch { start: point(-10), end: point(-8) },
            SearchMatch { start: point(-3), end: point(-1) },
        ];
        assert_eq!(initial_match(&hits, point(-9)), Some(0));
        assert_eq!(initial_match(&hits, point(-7)), Some(1));
        assert_eq!(initial_match(&hits, point(0)), Some(1));
    }

    #[test]
    fn stale_results_are_cleared_and_limits_are_visible() {
        let mut search = Search {
            active: true,
            query: "test".into(),
            results: SearchResults { truncated: true, ..Default::default() },
            ..Default::default()
        };
        assert!(search.status().contains("partial"));
        search.invalidate();
        assert!(search.current().is_none());
        assert!(search.status().contains("Output changed"));
    }

    #[test]
    fn german_folding_highlights_original_cells_and_exact_mode_preserves_spelling() {
        let mut config = mechanic_config::Config::default();
        config.shell.program = "/bin/cat".into();
        config.shell.integration = false;
        let mut terminal = Terminal::new(
            &config,
            mechanic_core::TerminalSize { columns: 40, rows: 4, ..Default::default() },
            std::sync::Arc::new(|| {}),
        )
        .unwrap();
        terminal.inject_local(
            "Straße STRASSE STRAẞE\r\nMu\u{308}ller MÜLLER Mueller Muller".as_bytes(),
        );
        let mut search = Search { active: true, ..Default::default() };
        search.set_query("STRASSE".into(), &terminal);
        assert_eq!(search.results.matches.len(), 3);
        for (hit, spelling) in search.results.matches.iter().zip(["Straße", "STRASSE", "STRAẞE"])
        {
            terminal.start_selection(hit.start, mechanic_core::GridSide::Left);
            terminal.update_selection(hit.end, mechanic_core::GridSide::Right);
            assert_eq!(terminal.selection_text().as_deref(), Some(spelling));
        }
        terminal.clear_selection();
        let mut grid = crate::convert::convert_grid(&terminal, &config.theme, true);
        let following = grid.cells[6].clone();
        search.highlight(&mut grid, &terminal, &config.theme);
        assert_eq!(grid.cells[4].character, 'ß');
        assert_eq!(grid.cells[4].bg, config.theme.selection.background);
        assert_eq!(grid.cells[6], following);
        search.set_case_sensitive(true, &terminal);
        assert_eq!(search.results.matches.len(), 1);
        assert_eq!(search.current().unwrap().start.column.0, 7);
        search.set_case_sensitive(false, &terminal);
        search.set_query("MÜLLER".into(), &terminal);
        assert_eq!(search.results.matches.len(), 2);
        let hit = search.current().unwrap();
        terminal.start_selection(hit.start, mechanic_core::GridSide::Left);
        terminal.update_selection(hit.end, mechanic_core::GridSide::Right);
        assert_eq!(terminal.selection_text().as_deref(), Some("Mu\u{308}ller"));
        search.set_case_sensitive(true, &terminal);
        assert_eq!(search.results.matches.len(), 1);
        search.set_query("\u{308}".into(), &terminal);
        assert_eq!(search.results.matches.len(), 1);
    }

    #[test]
    fn unicode_highlights_preserve_cells_selection_cursor_and_refresh() {
        let mut config = mechanic_config::Config::default();
        config.shell.program = "/bin/cat".into();
        config.shell.integration = false;
        let mut terminal = Terminal::new(
            &config,
            mechanic_core::TerminalSize { columns: 18, rows: 4, ..Default::default() },
            std::sync::Arc::new(|| {}),
        )
        .unwrap();
        terminal.inject_local("界 cafe\u{301} العربية\r\n界 cafe\u{301} العربية".as_bytes());
        let mut search = Search { active: true, ..Default::default() };
        search.set_query("界".into(), &terminal);
        assert_eq!(search.results.matches.len(), 2);
        let theme = &config.theme;
        let mut grid = crate::convert::convert_grid(&terminal, theme, true);
        let originals = grid.cells.clone();
        search.highlight(&mut grid, &terminal, theme);
        assert_eq!(grid.cells[0].bg, theme.selection.background);
        assert_eq!(grid.cells[1].bg, theme.selection.background);
        for (after, before) in grid.cells.iter().zip(&originals) {
            assert_eq!(after.character, before.character);
            assert_eq!(after.zerowidth, before.zerowidth);
            assert_eq!(after.flags, before.flags);
        }
        terminal.start_selection(
            GridPoint::new(GridLine(1), GridColumn(0)),
            mechanic_core::GridSide::Left,
        );
        terminal.update_selection(
            GridPoint::new(GridLine(1), GridColumn(1)),
            mechanic_core::GridSide::Right,
        );
        let mut grid = crate::convert::convert_grid(&terminal, theme, true);
        let selected = grid.cells[18..20].to_vec();
        let cursor = grid.cursor_position.1 * grid.cols + grid.cursor_position.0;
        let cursor_cell = grid.cells[cursor].clone();
        search.highlight(&mut grid, &terminal, theme);
        assert_eq!(grid.cells[18..20], selected);
        assert_eq!(grid.cells[cursor], cursor_cell);
        terminal.inject_local("\r\n界".as_bytes());
        assert!(search.invalidate());
        assert!(!search.invalidate());
        search.navigate(&terminal, false);
        assert_eq!(search.results.matches.len(), 3);
        assert!(!search.stale);
        search.set_query("cafe\u{301}".into(), &terminal);
        assert_eq!(search.results.matches.len(), 2);
        search.set_query("العربية".into(), &terminal);
        assert_eq!(search.results.matches.len(), 2);
    }
}
