//! PTY spawning and I/O.
//!
//! Wraps `alacritty_terminal::tty` to open a pseudo-terminal, spawn the
//! configured shell, and hand back a [`PtyHandle`] through which the caller
//! can read output bytes and write keyboard input.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::tty::{self, EventedReadWrite as _, Options, Shell};
use crossbeam_channel::{Receiver, Sender, bounded};
use log::error;

use mechanic_config::Config;

use crate::PtyWaker;
use crate::TerminalSize;
use crate::error::TerminalError;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Capacity of the channel that carries PTY output bytes to the main thread.
const CHANNEL_CAPACITY: usize = 256;

/// Counter used to give each PTY reader thread a unique name for debuggers /
/// Activity Monitor.  `Relaxed` ordering is sufficient — this is purely for
/// human-readable thread naming, not synchronisation.
static PTY_READER_ID: AtomicU64 = AtomicU64::new(0);

/// Maximum bytes read from the PTY per iteration.
const READ_BUF_SIZE: usize = 0x10_0000; // 1 MiB

// ── PtyHandle ─────────────────────────────────────────────────────────────────

/// A handle to an active PTY session.
///
/// The PTY lives in a background reader thread which pushes byte chunks into a
/// [`crossbeam_channel`].  The main thread calls [`PtyHandle::recv`] to drain
/// that channel and feed the bytes to the terminal parser.  Keyboard input is
/// written directly via [`PtyHandle::write`].
pub struct PtyHandle {
    /// Sender side of the write pipe to the PTY.
    writer: std::fs::File,
    /// Receiver side of the channel that carries PTY output to the main thread.
    pub(crate) rx: Receiver<Vec<u8>>,
    /// Background reader thread handle (kept alive for Drop purposes).
    _reader_thread: JoinHandle<()>,
}

impl PtyHandle {
    /// Spawn a PTY from the given configuration and terminal size.
    ///
    /// This opens the pseudo-terminal device, forks the shell, and starts a
    /// background thread that continuously drains the PTY master file
    /// descriptor and sends byte chunks through an internal channel.
    ///
    /// `waker` is invoked by the reader thread after every successful
    /// chunk send.  The application layer wires this to its event-loop
    /// proxy so the main loop can sleep at idle and wake promptly on
    /// PTY output.  Tests can pass `Arc::new(|| {})`.
    pub fn spawn(
        config: &Config,
        size: TerminalSize,
        waker: PtyWaker,
    ) -> Result<Self, TerminalError> {
        let window_size = size.to_window_size();

        // Build alacritty_terminal PTY options.
        let mut options = Options::default();
        let shell_program = config.shell.program.clone();
        if !shell_program.is_empty() {
            options.shell = Some(Shell::new(shell_program, vec![]));
        }

        // Set standard terminal env vars.
        tty::setup_env();

        // Spawn the PTY + child process. Window ID 0 is fine for a
        // non-X11/Wayland terminal.
        let pty = tty::new(&options, window_size, 0).map_err(TerminalError::PtySpawn)?;

        // Clone the master file for writing.  `Pty::file()` gives us a shared
        // reference, so we use `try_clone` to get an owned `File`.
        let writer = pty.file().try_clone().map_err(|e| {
            TerminalError::Io(io::Error::new(e.kind(), format!("clone PTY fd: {e}")))
        })?;

        let (tx, rx) = bounded::<Vec<u8>>(CHANNEL_CAPACITY);

        let reader_thread = Self::start_reader(pty, tx, waker);

        Ok(Self { writer, rx, _reader_thread: reader_thread })
    }

    /// Write `data` to the PTY (keyboard / paste input).
    pub fn write(&mut self, data: &[u8]) -> Result<(), TerminalError> {
        self.writer.write_all(data).map_err(TerminalError::Io)
    }

    /// Return the raw file descriptor of the PTY master (for `TIOCSWINSZ`).
    pub(crate) fn writer_fd(&self) -> std::os::unix::io::RawFd {
        use std::os::unix::io::AsRawFd as _;
        self.writer.as_raw_fd()
    }

    // ── Private ──────────────────────────────────────────────────────────────

    /// Spin up the background reader thread.
    fn start_reader(
        mut pty: tty::Pty,
        tx: Sender<Vec<u8>>,
        waker: PtyWaker,
    ) -> JoinHandle<()> {
        let id = PTY_READER_ID.fetch_add(1, Ordering::Relaxed);
        thread::Builder::new()
            .name(format!("mechanic-pty-reader-{id}"))
            .spawn(move || {
                let mut buf = vec![0u8; READ_BUF_SIZE];
                loop {
                    match pty.reader().read(&mut buf) {
                        Ok(0) => {
                            // EOF — the shell has exited.  Fire the waker
                            // one last time so the main loop redraws and
                            // observes the resulting Exit event this frame
                            // rather than waiting for user input.
                            waker();
                            break;
                        }
                        Ok(n) => {
                            let chunk = buf[..n].to_vec();
                            if tx.send(chunk).is_err() {
                                // Receiver dropped — terminal is shutting down.
                                break;
                            }
                            // Wake the main loop so it processes the bytes
                            // promptly even if it was sleeping on `Wait`.
                            // Coalesces naturally on the winit side: a burst
                            // of sends results in one redraw, not N.
                            waker();
                        }
                        Err(ref e)
                            if e.kind() == io::ErrorKind::WouldBlock
                                || e.kind() == io::ErrorKind::Interrupted =>
                        {
                            // Transient — try again.
                            continue;
                        }
                        Err(e) => {
                            // On Linux an EIO can arrive when the slave side
                            // closes before SIGCHLD; treat it as EOF.
                            #[cfg(target_os = "linux")]
                            if e.raw_os_error() == Some(libc::EIO) {
                                break;
                            }
                            error!("PTY read error: {e}");
                            break;
                        }
                    }
                }
            })
            .expect("failed to spawn PTY reader thread")
    }
}

// ── TerminalSize helpers ───────────────────────────────────────────────────────

impl TerminalSize {
    /// Convert to the `WindowSize` type expected by `alacritty_terminal`.
    pub(crate) fn to_window_size(self) -> WindowSize {
        WindowSize {
            num_lines: self.rows as u16,
            num_cols: self.columns as u16,
            cell_width: self.cell_width as u16,
            cell_height: self.cell_height as u16,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_size_to_window_size() {
        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let ws = size.to_window_size();
        assert_eq!(ws.num_cols, 80);
        assert_eq!(ws.num_lines, 24);
        assert_eq!(ws.cell_width, 8);
        assert_eq!(ws.cell_height, 16);
    }
}
