pub const CASES: &[&str] =
    &["ascii", "wrap", "unicode", "sgr", "scrollback", "scroll-region", "repaint", "sparse"];
pub const VERSION: u32 = 1;

pub fn payload(case: &str, bytes: usize, cols: usize, rows: usize) -> Vec<u8> {
    let unit = match case {
        "ascii" | "scrollback" => format!("{}\r\n", "0123456789abcdef".repeat(4)),
        "wrap" => format!("{}\r\n", "x".repeat(cols * 4 + 7)),
        "unicode" => "café e\u{301} Ελληνικά Русский العربية 中文 日本語 🦀\r\n".into(),
        "sgr" => {
            (0..64)
                .map(|i| {
                    format!(
                        "\x1b[38;2;{};{};{}m\x1b[48;5;{}mcolor\x1b[0m ",
                        i * 3,
                        255 - i * 3,
                        i * 2,
                        i
                    )
                })
                .collect::<String>()
                + "\r\n"
        }
        "scroll-region" => {
            format!("\x1b[2;{}r\x1b[{};1Hregion scroll\r\n\x1b[1S\x1b[1T", rows - 1, rows - 1)
        }
        // Repeat two distinct frames as a complete cycle so successive
        // records change displayed content even at the cycle boundary.
        "repaint" => ['#', '%']
            .into_iter()
            .flat_map(|symbol| {
                (1..=rows).map(move |row| {
                    format!(
                        "\x1b[{row};1H\x1b[2K{:0>6} {}",
                        row,
                        symbol.to_string().repeat(cols - 8)
                    )
                })
            })
            .collect(),
        "sparse" => (0..2)
            .flat_map(|phase| {
                (1..=rows).map(move |row| {
                    format!("\x1b[{row};{}H{:02}\x1b[K", row % (cols - 3) + 1, (row + phase) % 100)
                })
            })
            .collect(),
        _ => unreachable!("validated case"),
    };
    // Repeat whole records so UTF-8 and escape sequences remain complete.
    unit.as_bytes().repeat(bytes.div_ceil(unit.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn payloads_are_deterministic_complete_records() {
        for case in CASES {
            let a = payload(case, 12345, 80, 24);
            assert!(a.len() >= 12345);
            assert!(std::str::from_utf8(&a).is_ok());
            assert_eq!(a, payload(case, 12345, 80, 24));
        }
    }

    #[test]
    fn successive_tui_frames_change_the_terminal_grid() {
        use alacritty_terminal::{
            Term,
            index::{Column, Line, Point},
            term::Config,
            vte::ansi::Processor,
        };
        use mechanic_core::EventProxy;

        for (case, marker, column, first, second) in [
            ("repaint", b"\x1b[1;1H".as_slice(), 7, '#', '%'),
            ("sparse", b"\x1b[1;2H".as_slice(), 2, '1', '2'),
        ] {
            let cycle = payload(case, 1, 80, 24);
            let boundary = cycle
                .windows(marker.len())
                .enumerate()
                .skip(1)
                .find(|(_, bytes)| *bytes == marker)
                .unwrap()
                .0;
            let mut term = Term::new(Config::default(), &crate::Size(80, 24), EventProxy::new());
            let mut parser: Processor = Processor::new();
            let point = Point::new(Line(0), Column(column));
            parser.advance(&mut term, &cycle[..boundary]);
            assert_eq!(term.grid()[point].c, first, "{case}: first frame");
            parser.advance(&mut term, &cycle[boundary..]);
            assert_eq!(term.grid()[point].c, second, "{case}: second frame");
            parser.advance(&mut term, &cycle[..boundary]);
            assert_eq!(term.grid()[point].c, first, "{case}: cycle boundary");
        }
    }
}
