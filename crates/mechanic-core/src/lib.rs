//! `mechanic-core` — terminal emulation and PTY management.
//!
//! This crate wraps [`alacritty_terminal`] to provide a high-level interface
//! for spawning a PTY, feeding its output to the terminal parser, and exposing
//! the resulting grid to a renderer.
//!
//! # Quick-start
//!
//! ```no_run
//! use mechanic_config::Config;
//! use mechanic_core::{Terminal, TerminalSize};
//!
//! let config = Config::default();
//! let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
//! let mut term = Terminal::new(&config, size).expect("failed to start terminal");
//!
//! loop {
//!     term.process_input();
//!     // pass term.grid() to the renderer …
//! }
//! ```

pub mod error;
pub mod event;
pub mod pty;
pub mod terminal;

pub use error::TerminalError;
pub use event::{EventProxy, TerminalEvent};
pub use terminal::Terminal;

// ── TerminalSize ──────────────────────────────────────────────────────────────

/// The dimensions of a terminal viewport, in character cells and pixels.
///
/// `cell_width` and `cell_height` are the size of a single character cell in
/// pixels.  They are forwarded to the PTY via `TIOCSWINSZ` so that pixel-aware
/// applications (e.g. image protocols) report the correct terminal size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    /// Width of the terminal in character columns.
    pub columns: usize,
    /// Height of the terminal in character rows.
    pub rows: usize,
    /// Width of a single character cell in pixels.
    pub cell_width: usize,
    /// Height of a single character cell in pixels.
    pub cell_height: usize,
}

impl Default for TerminalSize {
    /// A sensible default: 80×24 with 8×16-pixel cells.
    fn default() -> Self {
        Self { columns: 80, rows: 24, cell_width: 8, cell_height: 16 }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_size_default() {
        let size = TerminalSize::default();
        assert_eq!(size.columns, 80);
        assert_eq!(size.rows, 24);
        assert_eq!(size.cell_width, 8);
        assert_eq!(size.cell_height, 16);
    }

    #[test]
    fn terminal_size_equality() {
        let a = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let b = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        assert_eq!(a, b);
    }
}
