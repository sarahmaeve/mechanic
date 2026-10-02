//! Bounded literal search over the active terminal grid and its scrollback.
//!
//! Softwrapped rows form one searchable line; hard line breaks end a match.
//! Case-insensitive search uses canonical caseless matching (NFD → full case
//! fold → NFD), with Unicode 16 folding data from `caseless` 0.2.2 and Unicode
//! 17 normalization data from `unicode-normalization` 0.1.25. Matches cover whole
//! terminal cells, so `ss` matches `ß`, while `s` does not match half of `ß` and
//! `u` does not match `ü`. Every mapped character retains its original cell
//! coordinates. Case-sensitive search matches the exact stored representation,
//! including individual combining marks, without normalization or folding.

use std::collections::VecDeque;

use alacritty_terminal::Grid;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use caseless::Caseless;
use unicode_normalization::UnicodeNormalization;

const MAX_QUERY_CHARS: usize = 4096;
const MAX_CELL_CHARS: usize = 4096;

/// Inclusive grid coordinates, including both columns of a wide character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchMatch {
    pub start: Point,
    pub end: Point,
}

impl SearchMatch {
    pub fn contains(&self, point: Point) -> bool {
        self.start <= point && point <= self.end
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    /// Maximum results to retain.
    pub max_matches: usize,
    /// Maximum grid cells to scan. Large histories retain the newest whole rows.
    /// Emitted Unicode characters also have a budget of four times this value.
    pub max_cells: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self { case_sensitive: false, max_matches: 10_000, max_cells: 2_000_000 }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchResults {
    /// Ordered from oldest to newest; overlapping literal matches are included.
    pub matches: Vec<SearchMatch>,
    /// Some history or matches were omitted because a budget was exceeded.
    pub truncated: bool,
    /// Queries exceeding 4096 raw or normalized/folded characters are not searched.
    pub query_too_long: bool,
}

/// Search the supplied active grid. Coordinates remain valid until it changes.
pub fn search_grid(grid: &Grid<Cell>, query: &str, options: SearchOptions) -> SearchResults {
    let mut results = SearchResults::default();
    // Bound raw input before normalization can buffer a long combining run.
    if query.chars().take(MAX_QUERY_CHARS + 1).count() > MAX_QUERY_CHARS {
        results.query_too_long = true;
        return results;
    }
    let needle: Vec<_> = if options.case_sensitive {
        query.chars().collect()
    } else if query.is_ascii() {
        query.chars().map(|c| c.to_ascii_lowercase()).collect()
    } else {
        query.chars().nfd().default_case_fold().nfd().take(MAX_QUERY_CHARS + 1).collect()
    };
    if needle.len() > MAX_QUERY_CHARS {
        results.query_too_long = true;
        return results;
    }
    if needle.is_empty() {
        return results;
    }

    let columns = grid.columns();
    if columns == 0 || grid.total_lines() == 0 {
        return results;
    }
    let rows = grid.total_lines().min(options.max_cells / columns);
    results.truncated = rows < grid.total_lines();
    if rows == 0 {
        return results;
    }
    let first = grid.screen_lines() as i32 - rows as i32;
    let last = grid.screen_lines() as i32 - 1;
    let mut matcher = Matcher::new(needle);
    let mut chars_left = options.max_cells.saturating_mul(4);

    for line in first..=last {
        for column in 0..columns {
            let point = Point::new(Line(line), Column(column));
            let cell = &grid[point];
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let end_column = if cell.flags.contains(Flags::WIDE_CHAR) {
                (column + 1).min(columns - 1)
            } else {
                column
            };
            let span =
                SearchMatch { start: point, end: Point::new(Line(line), Column(end_column)) };
            let marks = cell.zerowidth().unwrap_or_default();
            // NFD sorts and buffers combining runs. Cap each original cell and
            // account for its raw input before allowing either normalizer to run.
            if marks.len() >= MAX_CELL_CHARS || marks.len() >= chars_left {
                results.truncated = true;
                return results;
            }
            let mut emit = |c, starts_cell, ends_cell| {
                if chars_left == 0 {
                    return false;
                }
                chars_left -= 1;
                if let Some(found) = matcher.feed(c, span, starts_cell, ends_cell) {
                    if results.matches.last() == Some(&found) {
                        return true;
                    }
                    if results.matches.len() == options.max_matches {
                        return false;
                    }
                    results.matches.push(found);
                }
                true
            };
            let characters = std::iter::once(cell.c).chain(marks.iter().copied());
            let completed = if options.case_sensitive {
                characters.into_iter().all(|c| emit(c, true, true))
            } else if cell.c.is_ascii() && marks.is_empty() {
                emit(cell.c.to_ascii_lowercase(), true, true)
            } else {
                let mut mapped = characters.nfd().default_case_fold().nfd().peekable();
                let mut first = true;
                let mut completed = true;
                while let Some(c) = mapped.next() {
                    if !emit(c, first, mapped.peek().is_none()) {
                        completed = false;
                        break;
                    }
                    first = false;
                }
                completed
            };
            if !completed {
                results.truncated = true;
                return results;
            }
        }
        if !grid[Line(line)][Column(columns - 1)].flags.contains(Flags::WRAPLINE) {
            matcher.reset();
        }
    }
    results
}

/// Streaming KMP with just enough coordinate history to identify a match.
struct Matcher {
    needle: Vec<char>,
    prefix: Vec<usize>,
    matched: usize,
    spans: VecDeque<(SearchMatch, bool)>,
}

impl Matcher {
    fn new(needle: Vec<char>) -> Self {
        let mut prefix = vec![0; needle.len()];
        let mut matched = 0;
        for i in 1..needle.len() {
            while matched > 0 && needle[i] != needle[matched] {
                matched = prefix[matched - 1];
            }
            if needle[i] == needle[matched] {
                matched += 1;
            }
            prefix[i] = matched;
        }
        Self { needle, prefix, matched: 0, spans: VecDeque::new() }
    }

    fn reset(&mut self) {
        self.matched = 0;
        self.spans.clear();
    }

    fn feed(
        &mut self,
        c: char,
        span: SearchMatch,
        starts_cell: bool,
        ends_cell: bool,
    ) -> Option<SearchMatch> {
        if self.spans.len() == self.needle.len() {
            self.spans.pop_front();
        }
        self.spans.push_back((span, starts_cell));
        while self.matched > 0 && c != self.needle[self.matched] {
            self.matched = self.prefix[self.matched - 1];
        }
        if c == self.needle[self.matched] {
            self.matched += 1;
        }
        if self.matched == self.needle.len() {
            self.matched = self.prefix[self.matched - 1];
            let (first, starts_cell) = self.spans.front().unwrap();
            (*starts_cell && ends_cell).then_some(SearchMatch { start: first.start, end: span.end })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::Term;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    struct Size {
        columns: usize,
        rows: usize,
    }

    impl Dimensions for Size {
        fn total_lines(&self) -> usize {
            self.rows
        }
        fn screen_lines(&self) -> usize {
            self.rows
        }
        fn columns(&self) -> usize {
            self.columns
        }
    }

    fn term(columns: usize, rows: usize, history: usize, text: &str) -> Term<VoidListener> {
        let config = Config { scrolling_history: history, ..Config::default() };
        let mut term = Term::new(config, &Size { columns, rows }, VoidListener);
        Processor::<StdSyncHandler>::new().advance(&mut term, text.as_bytes());
        term
    }

    fn search(term: &Term<VoidListener>, query: &str) -> SearchResults {
        search_grid(term.grid(), query, SearchOptions::default())
    }

    fn span(line: i32, first: usize, last: usize) -> SearchMatch {
        SearchMatch {
            start: Point::new(Line(line), Column(first)),
            end: Point::new(Line(line), Column(last)),
        }
    }

    #[test]
    fn literal_overlapping_case_and_empty_query() {
        let term = term(20, 2, 0, "aAa.aAa[one]");
        assert_eq!(
            search(&term, "aa").matches,
            vec![span(0, 0, 1), span(0, 1, 2), span(0, 4, 5), span(0, 5, 6)]
        );
        assert_eq!(search(&term, ".[one]").matches.len(), 0);
        assert_eq!(search(&term, "[one]").matches, vec![span(0, 7, 11)]);
        assert!(search(&term, "").matches.is_empty());
        let sensitive = SearchOptions { case_sensitive: true, ..SearchOptions::default() };
        assert_eq!(
            search_grid(term.grid(), "aA", sensitive).matches,
            vec![span(0, 0, 1), span(0, 4, 5)]
        );
    }

    #[test]
    fn unicode_combining_and_wide_cell_coordinates() {
        let term = term(20, 2, 0, "x界e\u{301}İΩ");
        assert_eq!(search(&term, "界e\u{301}").matches, vec![span(0, 1, 3)]);
        assert_eq!(search(&term, "界").matches, vec![span(0, 1, 2)]);
        assert!(search(&term, "\u{301}").matches.is_empty());
        let sensitive = SearchOptions { case_sensitive: true, ..Default::default() };
        assert_eq!(search_grid(term.grid(), "\u{301}", sensitive).matches, vec![span(0, 3, 3)]);
        assert_eq!(search(&term, "i\u{307}ω").matches, vec![span(0, 4, 5)]);
        assert_eq!(search(&term, "é").matches, vec![span(0, 3, 3)]);
    }

    #[test]
    fn soft_wraps_join_and_hard_breaks_separate() {
        let wrapped = term(4, 3, 0, "abcdef");
        assert_eq!(
            search(&wrapped, "cde").matches,
            vec![SearchMatch {
                start: Point::new(Line(0), Column(2)),
                end: Point::new(Line(1), Column(0))
            }]
        );
        let hard = term(4, 4, 0, "abcd\r\nef\r\n\r\ngh");
        assert!(search(&hard, "cde").matches.is_empty());
        assert!(search(&hard, "efgh").matches.is_empty());
    }

    #[test]
    fn wide_wrap_spacer_does_not_insert_a_space() {
        let term = term(4, 3, 0, "abc界z");
        let found = search(&term, "c界z").matches;
        assert_eq!(
            found,
            vec![SearchMatch {
                start: Point::new(Line(0), Column(2)),
                end: Point::new(Line(1), Column(2))
            }]
        );
    }

    #[test]
    fn history_and_active_alternate_screen() {
        let mut term = term(8, 2, 20, "old\r\nmid\r\nnew");
        assert_eq!(search(&term, "old").matches, vec![span(-1, 0, 2)]);
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\x1b[?1049halt");
        assert!(search(&term, "old").matches.is_empty());
        assert_eq!(search(&term, "alt").matches.len(), 1);
        Processor::<StdSyncHandler>::new().advance(&mut term, b"\x1b[?1049l");
        assert_eq!(search(&term, "old").matches.len(), 1);
    }

    #[test]
    fn budgets_report_omissions_and_reject_large_queries() {
        let term = term(8, 2, 20, "old\r\naaaa\r\nnew");
        let options = SearchOptions { max_cells: 16, ..SearchOptions::default() };
        let result = search_grid(term.grid(), "new", options);
        assert_eq!(result.matches, vec![span(1, 0, 2)]);
        assert!(result.truncated);
        let options = SearchOptions { max_matches: 2, ..SearchOptions::default() };
        let result = search_grid(term.grid(), "aa", options);
        assert_eq!(result.matches.len(), 2);
        assert!(result.truncated);
        let result = search(&term, &"a".repeat(4097));
        assert!(result.query_too_long);
        assert!(result.matches.is_empty());
        assert!(
            search_grid(term.grid(), "new", SearchOptions { max_cells: 0, ..options }).truncated
        );
    }

    #[test]
    fn exactly_matching_result_cap_is_complete() {
        let term = term(8, 1, 0, "aa");
        let options = SearchOptions { max_matches: 1, ..SearchOptions::default() };
        let result = search_grid(term.grid(), "aa", options);
        assert_eq!(result.matches.len(), 1);
        assert!(!result.truncated);
    }

    #[test]
    fn pathological_combining_cells_are_bounded() {
        let mut term = term(2, 1, 0, "a");
        for _ in 0..100 {
            term.grid_mut()[Line(0)][Column(0)].push_zerowidth('\u{301}');
        }
        let options = SearchOptions { max_cells: 2, ..SearchOptions::default() };
        let result = search_grid(term.grid(), "z", options);
        assert!(result.truncated);
    }

    // Source text deliberately retains its original capitalization and marks;
    // extraction checks that Unicode search coordinates describe those cells.
    const LANGUAGES: [(&str, &str, &str, &str); 10] = [
        ("Russian", "Привет мир", "ПРИВЕТ МИР", "пока"),
        ("Ukrainian", "Привіт світе", "ПРИВІТ СВІТЕ", "до побачення"),
        ("Japanese", "こんにちは世界", "こんにちは世界", "さようなら"),
        ("Chinese", "你好世界", "你好世界", "再见"),
        ("Arabic", "مرحبًا بالعالم", "مرحبًا بالعالم", "وداعًا"),
        ("French", "Été à Montréal", "ÉTÉ À MONTRÉAL", "demain"),
        ("German", "Straße Grüße", "STRASSE GRÜSSE", "morgen"),
        ("Spanish", "Mañana corazón", "MAÑANA CORAZÓN", "adiós"),
        ("Portuguese", "Olá ação", "OLÁ AÇÃO", "adeus"),
        ("Italian", "Città perché", "CITTÀ PERCHÉ", "domani"),
    ];

    #[test]
    fn ten_languages_find_original_text_across_varied_softwraps() {
        for (language, original, query, absent) in LANGUAGES {
            for columns in [3, 5, 9, 17] {
                // The prefix puts a wide glyph one column from the right edge
                // for width 3, exercising its leading wrap spacer as well.
                let terminal = term(columns, 40, 100, &format!("xy{original}!"));
                let result = search(&terminal, query);
                assert!(!result.truncated, "{language}, width {columns}");
                assert_eq!(result.matches.len(), 1, "{language}, width {columns}");
                let found = result.matches[0];
                assert_eq!(
                    terminal.bounds_to_string(found.start, found.end),
                    original,
                    "{language}, width {columns}: {found:?}"
                );
                assert!(search(&terminal, absent).matches.is_empty(), "{language}");
                if original != query {
                    let sensitive = SearchOptions { case_sensitive: true, ..Default::default() };
                    assert!(
                        search_grid(terminal.grid(), query, sensitive).matches.is_empty(),
                        "{language}"
                    );
                }
            }
        }
    }

    #[test]
    fn ten_languages_do_not_cross_hard_line_breaks() {
        for (language, original, query, _) in LANGUAGES {
            let middle = original.char_indices().nth(original.chars().count() / 2).unwrap().0;
            let text = format!("{}\r\n{}", &original[..middle], &original[middle..]);
            let terminal = term(40, 4, 0, &text);
            assert!(search(&terminal, query).matches.is_empty(), "{language}");
        }
    }

    #[test]
    fn french_accents_are_canonical_in_caseless_mode_and_exact_when_sensitive() {
        let composed = term(8, 3, 0, "été");
        let decomposed = term(2, 4, 0, "e\u{301}te\u{301}");
        assert_eq!(search(&composed, "ÉTÉ").matches, vec![span(0, 0, 2)]);
        let found = search(&decomposed, "E\u{301}TE\u{301}").matches;
        assert_eq!(found.len(), 1);
        assert_eq!(decomposed.bounds_to_string(found[0].start, found[0].end), "e\u{301}te\u{301}");
        assert_eq!(search(&composed, "e\u{301}te\u{301}").matches.len(), 1);
        assert_eq!(search(&decomposed, "été").matches.len(), 1);
        let sensitive = SearchOptions { case_sensitive: true, ..Default::default() };
        assert!(search_grid(composed.grid(), "e\u{301}te\u{301}", sensitive).matches.is_empty());
        assert!(search_grid(decomposed.grid(), "été", sensitive).matches.is_empty());
        assert!(search(&composed, "ete").matches.is_empty());
    }

    #[test]
    fn german_full_folds_preserve_original_spans_across_wraps() {
        for original in ["Straße", "STRAẞE", "STRASSE", "strasse"] {
            for query in ["Straße", "STRAẞE", "STRASSE", "strasse"] {
                for columns in [3, 5, 8] {
                    let terminal = term(columns, 20, 100, &format!("xy{original}!"));
                    let result = search(&terminal, query);
                    assert_eq!(result.matches.len(), 1, "{original}/{query}, width {columns}");
                    let found = result.matches[0];
                    assert_eq!(terminal.bounds_to_string(found.start, found.end), original);
                }
            }
        }
        let terminal = term(12, 2, 0, "ß ẞ ss SS");
        assert_eq!(
            search(&terminal, "ss").matches,
            vec![span(0, 0, 0), span(0, 2, 2), span(0, 4, 5), span(0, 7, 8)]
        );
        assert_eq!(search(&terminal, "ß").matches, search(&terminal, "ss").matches);
        assert_eq!(
            search(&terminal, "s").matches,
            vec![span(0, 4, 4), span(0, 5, 5), span(0, 7, 7), span(0, 8, 8)]
        );
        let terminal = term(12, 1, 0, "xßy");
        for partial in ["xs", "sy", "s"] {
            assert!(search(&terminal, partial).matches.is_empty(), "{partial}");
        }
        assert_eq!(search(&terminal, "xssy").matches, vec![span(0, 0, 2)]);
    }

    #[test]
    fn overlapping_folds_recover_after_rejected_partial_cells() {
        let terminal = term(12, 1, 0, "ßßß");
        assert_eq!(search(&terminal, "ssss").matches, vec![span(0, 0, 1), span(0, 1, 2)]);
        let terminal = term(12, 1, 0, "ßssß");
        assert_eq!(
            search(&terminal, "ss").matches,
            vec![span(0, 0, 0), span(0, 1, 2), span(0, 3, 3)]
        );
        assert_eq!(search(&terminal, "ssss").matches, vec![span(0, 0, 2), span(0, 1, 3)]);
        let terminal = term(12, 1, 0, "xßss");
        assert_eq!(search(&terminal, "ss").matches, vec![span(0, 1, 1), span(0, 2, 3)]);
        assert_eq!(search(&terminal, "s").matches, vec![span(0, 2, 2), span(0, 3, 3)]);
        assert_eq!(search(&terminal, "xss").matches, vec![span(0, 0, 1)]);
    }

    #[test]
    fn german_umlauts_remain_meaningful_and_canonical() {
        for original in ["Müller Grüße schön", "Mu\u{308}ller Gru\u{308}ße scho\u{308}n"] {
            for columns in [3, 7, 20] {
                let terminal = term(columns, 20, 100, original);
                for query in ["MÜLLER GRÜSSE SCHÖN", "MU\u{308}LLER GRU\u{308}SSE SCHO\u{308}N"]
                {
                    let found = search(&terminal, query).matches;
                    assert_eq!(found.len(), 1, "{original}/{query}");
                    assert_eq!(terminal.bounds_to_string(found[0].start, found[0].end), original);
                }
                for absent in ["Muller", "Mueller", "u", "o", "\u{308}"] {
                    assert!(search(&terminal, absent).matches.is_empty(), "{original}/{absent}");
                }
            }
        }
        let terminal = term(20, 2, 0, "Müller Mu\u{308}ller Straße STRASSE");
        let sensitive = SearchOptions { case_sensitive: true, ..Default::default() };
        assert_eq!(search_grid(terminal.grid(), "Straße", sensitive).matches.len(), 1);
        assert_eq!(search_grid(terminal.grid(), "STRASSE", sensitive).matches.len(), 1);
        assert!(search_grid(terminal.grid(), "straße", sensitive).matches.is_empty());
        assert_eq!(search_grid(terminal.grid(), "Müller", sensitive).matches.len(), 1);
        assert_eq!(search_grid(terminal.grid(), "Mu\u{308}ller", sensitive).matches.len(), 1);
    }

    #[test]
    fn full_unicode_folding_and_canonical_order_are_not_german_special_cases() {
        let terminal = term(20, 3, 0, "Σ ς σ ﬃ a\u{315}\u{300}");
        assert_eq!(search(&terminal, "σ").matches.len(), 3);
        assert_eq!(search(&terminal, "ffi").matches, vec![span(0, 6, 6)]);
        assert!(search(&terminal, "ff").matches.is_empty());
        let found = search(&terminal, "A\u{300}\u{315}").matches;
        assert_eq!(found, vec![span(0, 8, 8)]);
        assert_eq!(terminal.bounds_to_string(found[0].start, found[0].end), "a\u{315}\u{300}");
        let terminal = term(10, 2, 0, "가");
        assert_eq!(search(&terminal, "가").matches.len(), 1);
    }

    #[test]
    fn normalization_is_bounded_before_buffering_long_mark_runs() {
        let mut terminal = term(2, 1, 0, "a");
        for _ in 0..MAX_CELL_CHARS {
            terminal.grid_mut()[Line(0)][Column(0)].push_zerowidth('\u{301}');
        }
        assert!(search(&terminal, "z").truncated);
        assert!(search(&terminal, &"\u{301}".repeat(MAX_QUERY_CHARS + 1)).query_too_long);
        // Full folding and canonical decomposition can expand an otherwise
        // valid raw query past the transformed-character limit.
        assert!(search(&terminal, &"ß".repeat(MAX_QUERY_CHARS / 2 + 1)).query_too_long);
        assert!(search(&terminal, &"ΐ".repeat(MAX_QUERY_CHARS / 3 + 1)).query_too_long);
    }

    /// Opt-in release timing; run serially without other build/render workloads.
    #[test]
    #[ignore = "release search latency benchmark; requires an otherwise idle machine"]
    fn diverse_ten_thousand_line_search_latency() {
        use std::hint::black_box;
        use std::time::Instant;

        const COLUMNS: usize = 120;
        for (name, line, query) in [
            (
                "ASCII",
                "build completed: needle target and additional terminal output",
                "needle target",
            ),
            ("Latin", "Été Straße mañana coração città: résultat trouvé", "RÉSULTAT TROUVÉ"),
            ("CJK", "日本語の検索結果 中文搜索结果 こんにちは世界 你好世界", "中文搜索结果"),
            ("Arabic", "مرحبًا بالعالم نتيجة البحث في سجل الطرفية", "نتيجة البحث"),
        ] {
            let mut input = String::new();
            for _ in 0..10_000 {
                input.push_str(line);
                input.push_str("\r\n");
            }
            let terminal = term(COLUMNS, 24, 10_000, &input);
            let complete = search(&terminal, query);
            assert_eq!(complete.matches.len(), 10_000, "{name}");
            assert!(!complete.truncated, "{name}");
            for options in [
                SearchOptions::default(),
                SearchOptions { max_cells: COLUMNS * 1000, ..Default::default() },
            ] {
                let expected = if options.max_cells == COLUMNS * 1000 { 999 } else { 10_000 };
                let mut durations = Vec::new();
                for sample in 0..9 {
                    let started = Instant::now();
                    let result = search_grid(black_box(terminal.grid()), black_box(query), options);
                    let elapsed = started.elapsed();
                    durations.push(elapsed);
                    eprintln!(
                        "search_sample,{name},{},{expected},{sample},{}",
                        options.max_cells,
                        elapsed.as_nanos()
                    );
                    assert_eq!(result.matches.len(), expected, "{name}");
                    assert_eq!(result.truncated, expected == 999, "{name}");
                    black_box(result);
                }
                durations.sort_unstable();
                eprintln!(
                    "search {name}: cell_budget={} hits={expected} min={:.3}ms median={:.3}ms max={:.3}ms",
                    options.max_cells,
                    durations[0].as_secs_f64() * 1000.0,
                    durations[4].as_secs_f64() * 1000.0,
                    durations[8].as_secs_f64() * 1000.0,
                );
            }
        }
    }
}
