//! OSC 8 targets at logical terminal coordinates.

use alacritty_terminal::Grid;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags, Hyperlink};
use mechanic_core::Terminal;

const PREVIEW_CHARACTERS: usize = 160;

/// An immutable hover target; URI validation and preview allocation happen once.
#[derive(Debug, Clone)]
pub struct LinkTarget {
    metadata: Hyperlink,
    preview: String,
    web_url: Option<String>,
}

impl LinkTarget {
    pub fn new(metadata: Hyperlink) -> Self {
        let preview = preview(metadata.uri());
        let web_url = validated_web_url(metadata.uri());
        Self { metadata, preview, web_url }
    }

    pub fn matches(&self, metadata: &Hyperlink) -> bool {
        self.metadata == *metadata
    }

    /// The original OSC 8 URI, preserved exactly for copying.
    pub fn uri(&self) -> &str {
        self.metadata.uri()
    }

    pub fn preview(&self) -> &str {
        &self.preview
    }

    /// A parsed HTTP(S) URL safe to pass as one native opener argument.
    pub fn web_url(&self) -> Option<&str> {
        self.web_url.as_deref()
    }

    pub fn can_open(&self) -> bool {
        self.web_url.is_some()
    }
}

/// Lookup is constant-time and clones only the hyperlink's shared metadata.
/// Coordinates must already be mapped from visual bidi order to logical cells.
pub fn at(terminal: &Terminal, logical_col: usize, viewport_row: usize) -> Option<Hyperlink> {
    at_grid(terminal.grid(), logical_col, viewport_row)
}

fn at_grid(grid: &Grid<Cell>, col: usize, row: usize) -> Option<Hyperlink> {
    if col >= grid.columns() || row >= grid.screen_lines() {
        return None;
    }
    let line = i32::try_from(row).ok()?.checked_sub(i32::try_from(grid.display_offset()).ok()?)?;
    let mut column = col;
    let cell = &grid[Line(line)][Column(column)];
    if cell.flags.intersects(Flags::HIDDEN | Flags::LEADING_WIDE_CHAR_SPACER) {
        return None;
    }
    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
        column = column.checked_sub(1)?;
    }
    let cell = &grid[Line(line)][Column(column)];
    if cell
        .flags
        .intersects(Flags::HIDDEN | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
    {
        return None;
    }
    cell.hyperlink()
}

fn bidi_control(character: char) -> bool {
    matches!(character, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{206f}')
}

fn validated_web_url(uri: &str) -> Option<String> {
    if uri.chars().any(|character| {
        character.is_control() || character.is_whitespace() || bidi_control(character)
    }) {
        return None;
    }
    // Do not let browser-style normalization turn relative or backslash
    // syntax into a different authority than the displayed OSC 8 address.
    let (scheme, rest) = uri.split_once("://")?;
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        || rest.split(['/', '?', '#']).next()?.is_empty()
        || uri.contains('\\')
    {
        return None;
    }
    let url = url::Url::parse(uri).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host_str().is_some()).then(|| url.to_string())
}

fn preview(uri: &str) -> String {
    let mut preview = String::new();
    let mut length = 0;
    for character in uri.chars() {
        let escaped;
        let mut encoded = [0; 4];
        let fragment =
            if character.is_control() || character.is_whitespace() || bidi_control(character) {
                escaped = character.escape_unicode().to_string();
                escaped.as_str()
            } else {
                character.encode_utf8(&mut encoded)
            };
        let count = fragment.chars().count();
        if length + count > PREVIEW_CHARACTERS {
            preview.push('…');
            return preview;
        }
        preview.push_str(fragment);
        length += count;
    }
    preview
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::Term;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::{Dimensions, Scroll};
    use alacritty_terminal::vte::ansi::Processor;

    struct Size {
        columns: usize,
        rows: usize,
    }
    impl Dimensions for Size {
        fn columns(&self) -> usize {
            self.columns
        }
        fn screen_lines(&self) -> usize {
            self.rows
        }
        fn total_lines(&self) -> usize {
            self.rows
        }
    }

    fn parsed(columns: usize, rows: usize, text: &str) -> Term<VoidListener> {
        let mut term = Term::new(Default::default(), &Size { columns, rows }, VoidListener);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, text.as_bytes());
        term
    }

    fn target(uri: &str) -> LinkTarget {
        LinkTarget::new(Hyperlink::new(Some("test"), uri.into()))
    }

    #[test]
    fn osc8_metadata_survives_wrap_and_close_without_plain_url_detection() {
        let term =
            parsed(4, 3, "\x1b]8;id=docs;https://example.com/actual\x1b\\abcdef\x1b]8;;\x1b\\gh");
        let grid = term.grid();
        let first = at_grid(grid, 0, 0).unwrap();
        assert_eq!(first.id(), "docs");
        assert_eq!(first.uri(), "https://example.com/actual");
        assert_eq!(at_grid(grid, 3, 0).unwrap(), first);
        assert_eq!(at_grid(grid, 0, 1).unwrap(), first);
        assert_eq!(at_grid(grid, 1, 1).unwrap(), first);
        assert!(at_grid(grid, 2, 1).is_none());
        assert!(at_grid(grid, 4, 0).is_none());
        assert!(at_grid(grid, 0, 3).is_none());
        assert!(at_grid(grid, usize::MAX, usize::MAX).is_none());
        let plain = parsed(80, 1, "https://example.com");
        assert!(at_grid(plain.grid(), 0, 0).is_none());
    }

    #[test]
    fn scrolled_viewport_maps_to_historical_link_cells() {
        let mut term =
            parsed(8, 2, "\x1b]8;id=old;https://example.com/old\x07old\x1b]8;;\x07\r\nnew\r\nlast");
        assert!(at_grid(term.grid(), 0, 0).is_none());
        term.scroll_display(Scroll::Delta(1));
        assert_eq!(term.grid().display_offset(), 1);
        assert_eq!(at_grid(term.grid(), 0, 0).unwrap().id(), "old");
        assert!(at_grid(term.grid(), 0, 1).is_none());
    }

    #[test]
    fn wide_spacers_combining_marks_and_hidden_cells_use_the_owner() {
        let term =
            parsed(8, 2, "\x1b]8;id=wide;https://example.com/wide\x07中a\u{301}\x1b[8mhidden");
        let first = at_grid(term.grid(), 0, 0).unwrap();
        assert_eq!(at_grid(term.grid(), 1, 0).unwrap(), first);
        assert_eq!(at_grid(term.grid(), 2, 0).unwrap(), first);
        assert_eq!(term.grid()[Line(0)][Column(2)].zerowidth(), Some(&['\u{301}'][..]));
        assert!(at_grid(term.grid(), 3, 0).is_none());
        let wrapped = parsed(3, 2, "ab\x1b]8;id=wrapped-wide;https://example.com/wide\x07中");
        assert!(wrapped.grid()[Line(0)][Column(2)].flags.contains(Flags::LEADING_WIDE_CHAR_SPACER));
        let owner = at_grid(wrapped.grid(), 0, 1).unwrap();
        // The leading spacer is empty wrap padding, not painted label text.
        assert!(at_grid(wrapped.grid(), 2, 0).is_none());
        assert_eq!(at_grid(wrapped.grid(), 1, 1).unwrap(), owner);
    }

    #[test]
    fn hover_identity_includes_osc8_id_and_exact_uri() {
        let first = Hyperlink::new(Some("one"), "https://example.com".into());
        let hover = LinkTarget::new(first.clone());
        assert!(hover.matches(&first));
        assert!(hover.matches(&Hyperlink::new(Some("one"), "https://example.com".into())));
        assert!(!hover.matches(&Hyperlink::new(Some("two"), "https://example.com".into())));
        assert!(!hover.matches(&Hyperlink::new(Some("one"), "https://example.com/other".into())));
    }

    #[test]
    fn opener_accepts_only_parsed_web_urls_without_invisible_input() {
        for uri in [
            "https://example.com/path?q=x#fragment",
            "HTTP://example.com:8080",
            "https://[::1]/",
            "https://例え.テスト/路径",
        ] {
            let target = target(uri);
            assert!(target.can_open(), "rejected {uri}");
            let parsed = url::Url::parse(target.web_url().unwrap()).unwrap();
            assert!(matches!(parsed.scheme(), "http" | "https"));
            assert_eq!(target.uri(), uri);
        }
        for uri in [
            "file:///tmp/document",
            "mailto:test@example.com",
            "javascript:alert(1)",
            "data:text/plain,hello",
            "https://",
            "http:relative",
            "https:example.com",
            "https:///example.com",
            "https://example.com\\path",
            "https://[invalid]/",
            "https://example.com:99999/",
            "https://example.com/with space",
            "https://example.com/\n",
            "https://example.com/\u{85}",
            "https://example.com/\u{202e}txt",
            "https://example.com/\u{2066}",
            "https://example.com/\u{061c}",
            "https://example.com/\u{200e}",
        ] {
            let target = target(uri);
            assert!(!target.can_open(), "accepted {uri:?}");
            assert!(target.web_url().is_none());
            assert_eq!(target.uri(), uri);
        }
    }

    #[test]
    fn previews_escape_controls_preserve_unicode_and_bound_long_targets() {
        let hover = target("https://example.com/路径\u{202e}\n\u{85} end");
        assert!(hover.preview().contains("路径"));
        for escaped in ["\\u{202e}", "\\u{a}", "\\u{85}", "\\u{20}"] {
            assert!(hover.preview().contains(escaped));
        }
        assert!(
            !hover
                .preview()
                .chars()
                .any(|character| character.is_control() || bidi_control(character))
        );
        let long_uri = format!("https://example.com/{}", "中".repeat(1000));
        let long = target(&long_uri);
        assert!(long.preview().ends_with('…'));
        assert!(long.preview().chars().count() <= PREVIEW_CHARACTERS + 1);
        assert_eq!(long.uri(), long_uri);
        let nonweb = target("file:///tmp/文件");
        assert_eq!(nonweb.preview(), nonweb.uri());
        assert!(!nonweb.can_open());
    }

    #[test]
    #[ignore = "manual constant-time OSC 8 lookup benchmark"]
    fn hyperlink_lookup_benchmark() {
        use std::hint::black_box;
        use std::time::{Duration, Instant};
        const ITERATIONS: usize = 1_000_000;
        let uri = format!("https://example.com/{}", "path/".repeat(120));
        let mut cases = Vec::new();
        for (columns, rows) in [(80, 24), (160, 100)] {
            for hit in [true, false] {
                let content = "x".repeat(columns * rows);
                let input =
                    if hit { format!("\x1b]8;id=benchmark;{uri}\x07{content}") } else { content };
                let term = parsed(columns, rows, &input);
                let grid = term.grid();
                for (col, row) in [(0, 0), (columns - 1, rows - 1)] {
                    let metadata = at_grid(grid, col, row);
                    assert_eq!(metadata.is_some(), hit);
                    if let Some(metadata) = metadata {
                        assert_eq!(metadata.uri(), uri, "OSC fixture must retain the complete URI");
                    }
                }
                let started = Instant::now();
                let mut warmup = 0;
                while started.elapsed() < Duration::from_millis(100) {
                    for index in warmup..warmup + 10_000 {
                        black_box(at_grid(grid, index % columns, index / columns % rows));
                    }
                    warmup += 10_000;
                }
                cases.push((columns, rows, hit, term, warmup));
            }
        }
        let mut csv = String::from(
            "workload,columns,rows,sample,lookups,warmup_lookups,warmup_ms,ns_per_lookup\n",
        );
        let mut timings = vec![Vec::new(); cases.len()];
        // Alternate case order to distribute residual CPU frequency/cache drift.
        for sample in 0..7 {
            for index in 0..cases.len() {
                let index = if sample % 2 == 0 { index } else { cases.len() - 1 - index };
                let (columns, rows, hit, term, warmup) = &cases[index];
                let grid = term.grid();
                let started = Instant::now();
                for lookup in 0..ITERATIONS {
                    black_box(at_grid(grid, lookup % columns, lookup / columns % rows));
                }
                let nanos = started.elapsed().as_secs_f64() * 1e9 / ITERATIONS as f64;
                timings[index].push(nanos);
                let workload = if *hit { "linked" } else { "unlinked" };
                csv.push_str(&format!(
                    "{workload},{columns},{rows},{sample},{ITERATIONS},{warmup},100,{nanos:.6}\n"
                ));
            }
        }
        for (case, mut timings) in cases.iter().zip(timings) {
            let (columns, rows, hit, _, _) = case;
            let workload = if *hit { "linked" } else { "unlinked" };
            timings.sort_by(f64::total_cmp);
            println!("{workload} {columns}x{rows}: {:.2} ns/lookup", timings[3]);
        }
        if let Ok(path) = std::env::var("MECHANIC_HYPERLINK_CSV") {
            std::fs::write(path, csv).unwrap();
        }
    }
}
