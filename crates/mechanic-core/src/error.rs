//! Error types for `mechanic-core`.

use std::fmt;
use std::io;

/// Errors that can occur during terminal creation and operation.
#[derive(Debug)]
pub enum TerminalError {
    /// Failed to spawn the PTY / shell process.
    PtySpawn(io::Error),
    /// An I/O error occurred while reading from or writing to the PTY.
    Io(io::Error),
    /// The terminal was given an invalid size (e.g. zero columns or rows).
    InvalidSize { columns: usize, rows: usize },
}

impl fmt::Display for TerminalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TerminalError::PtySpawn(e) => write!(f, "failed to spawn PTY: {e}"),
            TerminalError::Io(e) => write!(f, "PTY I/O error: {e}"),
            TerminalError::InvalidSize { columns, rows } => {
                write!(f, "invalid terminal size: {columns}x{rows}")
            }
        }
    }
}

impl std::error::Error for TerminalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TerminalError::PtySpawn(e) | TerminalError::Io(e) => Some(e),
            TerminalError::InvalidSize { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_invalid_size() {
        let err = TerminalError::InvalidSize { columns: 0, rows: 0 };
        assert!(err.to_string().contains("invalid terminal size"));
    }

    #[test]
    fn display_io_error() {
        let err = TerminalError::Io(io::Error::new(io::ErrorKind::BrokenPipe, "pipe"));
        assert!(err.to_string().contains("PTY I/O error"));
    }

    #[test]
    fn display_pty_spawn() {
        let err = TerminalError::PtySpawn(io::Error::new(io::ErrorKind::NotFound, "no shell"));
        assert!(err.to_string().contains("failed to spawn PTY"));
    }
}
