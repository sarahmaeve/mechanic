//! `mechanic-core` — terminal emulation and PTY management.

pub mod error;
pub mod event;
pub mod paste;
pub mod pty;
pub mod search;
mod shell_integration;
pub mod shell_state;
pub mod terminal;

pub use error::TerminalError;
pub use event::{EventProxy, TerminalEvent};
pub use shell_state::{CommandCompletion, ShellCommand, ShellIntegration, ShellPosition};
pub use terminal::{GridColumn, GridLine, GridPoint, GridSide, MouseProtocol, Terminal};

/// Wake the main loop after PTY output, exit, or transport failure.
pub type PtyWaker = std::sync::Arc<dyn Fn() + Send + Sync + 'static>;

/// The dimensions of a terminal viewport, in character cells and pixels.
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
    /// 80 columns by 24 rows, with 8 by 16 pixel cells.
    fn default() -> Self {
        Self { columns: 80, rows: 24, cell_width: 8, cell_height: 16 }
    }
}

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
