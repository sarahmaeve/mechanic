//! PTY spawning and I/O.
//!
//! Wraps `alacritty_terminal::tty` to open a pseudo-terminal, spawn the
//! configured shell, and hand back a [`PtyHandle`] through which the caller
//! can read output bytes and write keyboard input.

use std::io::{self, Read, Write};
use std::os::unix::process::ExitStatusExt as _;
use std::process::ExitStatus;
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
    /// One-shot channel carrying the child's exit status, populated by
    /// the reader thread after it sees EOF on the PTY master and reaps
    /// the child with `waitpid`.
    ///
    /// Without this channel the child-exit event was lost: we don't run
    /// `alacritty_terminal`'s own event loop, so no `ChildExit` event is
    /// ever synthesised for us.  The main loop would see a silent EOF
    /// and leave the window open forever when the user typed `exit`.
    ///
    /// Capacity 1 because the shell can only exit once.  The inner
    /// `Option` is `None` if `waitpid` failed (should be impossible in
    /// practice — if it does, callers treat it the same as a successful
    /// exit with no status).
    pub(crate) exit_rx: Receiver<Option<ExitStatus>>,
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

        // Clear O_NONBLOCK on the master fd.
        //
        // alacritty_terminal sets non-blocking I/O on the master (see
        // tty/unix.rs in that crate) because its own event loop uses
        // `mio` to poll the fd and wants `read()` to return immediately
        // when no data is available.  We use a different architecture:
        // a dedicated reader thread whose only job is to pump bytes
        // into a channel.  That thread is happy to sleep in the kernel
        // waiting for data — exactly what blocking reads give it.
        //
        // Without this fix the reader thread hits `WouldBlock` on every
        // read against an idle shell, `continue`s, and spins a core at
        // 100 %.  The cost was invisible while the main loop was also
        // busy-polling at 60-120 FPS; once the main loop learned to
        // sleep on `ControlFlow::Wait` (#9), the reader thread's spin
        // became the dominant CPU draw.
        unsafe {
            use std::os::unix::io::AsRawFd as _;
            let fd = pty.file().as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFL, 0);
            if flags >= 0 {
                let _ = libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
            }
        }

        // Clone the master file for writing.  `Pty::file()` gives us a shared
        // reference, so we use `try_clone` to get an owned `File`.  The
        // cleared O_NONBLOCK above is shared via the open file
        // description, so the cloned writer is also blocking.
        let writer = pty.file().try_clone().map_err(|e| {
            TerminalError::Io(io::Error::new(e.kind(), format!("clone PTY fd: {e}")))
        })?;

        let (tx, rx) = bounded::<Vec<u8>>(CHANNEL_CAPACITY);
        let (exit_tx, exit_rx) = bounded::<Option<ExitStatus>>(1);

        let reader_thread = Self::start_reader(pty, tx, exit_tx, waker);

        Ok(Self { writer, rx, exit_rx, _reader_thread: reader_thread })
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
    ///
    /// When the PTY master returns EOF — the canonical signal that the
    /// child shell has closed its end and exited — the thread reaps the
    /// child via `waitpid` so the exit status is recovered before
    /// `Pty::drop` calls `Child::wait` and discards it.  The status is
    /// delivered to the main thread via `exit_tx` and a final `waker()`
    /// call ensures the main loop wakes promptly to observe it rather
    /// than waiting for the user to bump the mouse.
    fn start_reader(
        mut pty: tty::Pty,
        tx: Sender<Vec<u8>>,
        exit_tx: Sender<Option<ExitStatus>>,
        waker: PtyWaker,
    ) -> JoinHandle<()> {
        // Capture the child PID before moving `pty` into the thread.
        // `Pty::child()` returns `&Child`, so we can't call
        // `Child::wait` (which needs `&mut`); `waitpid` on the PID works
        // on any thread and reaps the process correctly.
        let child_pid = pty.child().id() as libc::pid_t;

        let id = PTY_READER_ID.fetch_add(1, Ordering::Relaxed);
        thread::Builder::new()
            .name(format!("mechanic-pty-reader-{id}"))
            .spawn(move || {
                let mut buf = vec![0u8; READ_BUF_SIZE];
                // `saw_eof` distinguishes "shell exited, capture status"
                // from "receiver dropped / fatal read error" — in the
                // latter the main thread is tearing down and there's no
                // point reaping or notifying.
                let mut saw_eof = false;
                loop {
                    match pty.reader().read(&mut buf) {
                        Ok(0) => {
                            // EOF — the shell has exited.
                            saw_eof = true;
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
                        Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {
                            // A signal interrupted the read; retry.  This is
                            // the only "transient" case we expect now that
                            // the fd is blocking — WouldBlock can no longer
                            // fire and was removed from this match.
                            continue;
                        }
                        Err(e) => {
                            // On Linux an EIO can arrive when the slave side
                            // closes before SIGCHLD; treat it as EOF.
                            #[cfg(target_os = "linux")]
                            if e.raw_os_error() == Some(libc::EIO) {
                                saw_eof = true;
                                break;
                            }
                            error!("PTY read error: {e}");
                            break;
                        }
                    }
                }

                if saw_eof {
                    let status = reap_child(child_pid);
                    // Send is best-effort: if the receiver has dropped
                    // (Terminal being torn down) we just discard.  The
                    // waker call still fires so any still-live main loop
                    // gets one chance to observe the exit.
                    let _ = exit_tx.send(status);
                    waker();
                }
            })
            .expect("failed to spawn PTY reader thread")
    }
}

// ── Child reaping ─────────────────────────────────────────────────────────────

/// Reap the child at `pid` and return its exit status.
///
/// Called after the PTY master returns EOF — at that point the child
/// has closed its slave fd, which on Unix means it has exited (or at
/// least detached from the session we care about), so `waitpid` will
/// not block meaningfully.  Once this call succeeds the kernel has
/// released the zombie; `Pty::drop` subsequently calls `Child::wait`,
/// which will return `ECHILD` (already reaped) and be silently
/// discarded — harmless.
///
/// Returns `None` if `waitpid` fails (e.g. another thread already
/// reaped the child, or the PID was never alive).  Callers treat
/// `None` the same as "shell exited, status unavailable".
fn reap_child(pid: libc::pid_t) -> Option<ExitStatus> {
    let mut raw_status: libc::c_int = 0;
    // Safety: `raw_status` is a local variable, pointer is valid for
    // the duration of the call; `waitpid` is a standard libc call.
    let res = unsafe { libc::waitpid(pid, &mut raw_status, 0) };
    if res <= 0 {
        // -1 = error (e.g. ECHILD); 0 = no status available.  Neither
        // gives us a usable status.
        return None;
    }
    Some(ExitStatus::from_raw(raw_status))
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
