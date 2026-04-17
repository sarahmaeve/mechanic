//! Terminal state wrapper.
//!
//! [`Terminal`] owns an [`alacritty_terminal::Term`] instance together with a
//! [`PtyHandle`] and an [`EventProxy`].  The main-thread render loop calls
//! [`Terminal::process_input`] to drain available PTY bytes and update the
//! terminal grid, then reads grid state through [`Terminal::grid`] and
//! title state through [`Terminal::title`].

use alacritty_terminal::Grid;
use alacritty_terminal::Term;
use alacritty_terminal::event::WindowSize;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::vte::ansi::{CursorShape, Processor};
use mechanic_config::Config;

use crate::PtyWaker;
use crate::TerminalSize;
use crate::error::TerminalError;
use crate::event::{EventProxy, TerminalEvent};
use crate::pty::PtyHandle;

// ── ProcessOutcome ────────────────────────────────────────────────────────────

/// Result of one [`Terminal::process_input`] call.
///
/// Summarizes the events that occurred while bytes were being drained
/// from the PTY and fed to the VTE parser.  The caller uses this to
/// react to things `process_input` shouldn't decide on its own — such
/// as whether to close the window on shell exit.
#[derive(Debug, Clone, Default)]
pub struct ProcessOutcome {
    /// `Some(status)` if the child shell process exited during this
    /// call.  `None` means the shell is still alive (or was already
    /// dead and the exit event was delivered on a previous call).
    ///
    /// The outer `Option` is "did the shell exit during this call".
    /// The inner `Option<ExitStatus>` mirrors the [`TerminalEvent::Exit`]
    /// payload — `Some(status)` for a real child exit, `None` for the
    /// library-internal `AlacrittyEvent::Exit` which carries no status.
    pub child_exit: Option<Option<std::process::ExitStatus>>,

    /// `true` if any PTY bytes were drained through the VTE parser
    /// during this call (i.e. the grid *may* have changed).  Callers
    /// use this to decide whether to do a full grid-conversion render
    /// or a cheap animation-only render that re-uses the previous
    /// frame's instance buffer.
    ///
    /// Conservative: set whenever the parser was fed anything, without
    /// diffing cells — a CSI query that produces a PtyWrite response
    /// and no visible change still trips the flag.  That's fine; the
    /// worst case is one wasted full render.
    pub grid_maybe_changed: bool,
}

// ── MouseProtocol ─────────────────────────────────────────────────────────────

/// Snapshot of the DECSET mouse-reporting flags the running program
/// has enabled.
///
/// Programs subscribe to mouse events via DECSET sequences:
///
/// | DECSET | Flag           | Semantics                                         |
/// |--------|----------------|---------------------------------------------------|
/// | 1000   | `report_click` | Button press & release                            |
/// | 1002   | `report_drag`  | Press/release + motion while a button is held     |
/// | 1003   | `report_motion`| All motion (with or without buttons)              |
/// | 1006   | `sgr`          | Use SGR encoding (`ESC [ < Cb ; Cx ; Cy M|m`)     |
///
/// When `sgr` is off and any `report_*` flag is set, the legacy X10
/// encoding (`ESC [ M Cb Cx Cy` with each value offset by `0x20`) is
/// expected.  Almost all modern programs set 1006 alongside 1000/1002.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseProtocol {
    /// DECSET 1000 — press/release events.
    pub report_click: bool,
    /// DECSET 1002 — press/release + motion while a button is held.
    pub report_drag: bool,
    /// DECSET 1003 — all motion events.
    pub report_motion: bool,
    /// DECSET 1006 — SGR encoding.  When false and any `report_*` is
    /// true, callers should fall back to the legacy X10 encoding.
    pub sgr: bool,
}

impl MouseProtocol {
    /// Returns `true` if any form of mouse reporting is active — i.e.
    /// the running program wants mouse events forwarded via the PTY.
    pub fn is_tracking(&self) -> bool {
        self.report_click || self.report_drag || self.report_motion
    }
}

// ── Bracketed-paste constants ─────────────────────────────────────────────────

/// DECSET 2004 start-of-paste marker, written as a raw byte slice so
/// [`Terminal::paste`] can splice it into the outgoing buffer without
/// any UTF-8 trip.
const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";

/// DECSET 2004 end-of-paste marker.  Paired with [`BRACKETED_PASTE_START`].
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

/// Total bytes the start+end markers add to a payload.  Used as a
/// capacity hint when allocating the outgoing buffer.
const BRACKETED_PASTE_WRAP_OVERHEAD: usize =
    BRACKETED_PASTE_START.len() + BRACKETED_PASTE_END.len();

// ── Terminal ──────────────────────────────────────────────────────────────────

/// A running terminal session.
///
/// # Threading model
///
/// PTY bytes are produced on a background reader thread (inside [`PtyHandle`])
/// and shipped to the main thread via a [`crossbeam_channel`].
/// [`Terminal::process_input`] drains that channel and feeds each chunk to the
/// VTE parser, which in turn calls into `Term` to update the grid.  All grid
/// access must therefore happen on the same thread that calls
/// `process_input`.
pub struct Terminal {
    /// The `alacritty_terminal` state machine and grid.
    term: Term<EventProxy>,
    /// PTY handle — owns the background reader thread and write file.
    pty: PtyHandle,
    /// Event proxy — receives terminal events from `Term`.
    event_proxy: EventProxy,
    /// VTE ANSI escape-sequence parser.
    parser: Processor,
    /// Current terminal title (maintained from [`TerminalEvent`] s).
    title: String,
    /// Current terminal size.
    size: TerminalSize,
}

impl Terminal {
    /// Create a new terminal with a PTY of the given size.
    ///
    /// This spawns the shell configured in `config`, sets up the PTY with
    /// `size`, and initialises the `alacritty_terminal` state machine.
    ///
    /// `waker` is a thread-safe callback invoked by the PTY reader
    /// thread whenever new shell output lands in the channel.  The
    /// application layer typically wires this to a winit event-loop
    /// proxy so the main loop can sleep at idle and wake promptly on
    /// PTY output.  Tests pass `Arc::new(|| {})`.
    pub fn new(
        config: &Config,
        size: TerminalSize,
        waker: PtyWaker,
    ) -> Result<Self, TerminalError> {
        if size.columns == 0 || size.rows == 0 {
            return Err(TerminalError::InvalidSize { columns: size.columns, rows: size.rows });
        }

        let event_proxy = EventProxy::new();

        // Build a `Term` config, threading the scrollback-lines knob
        // through from our user-facing `TerminalConfig` into alacritty's
        // internal config.  Other fields keep alacritty's defaults.
        let term_config =
            TermConfig { scrolling_history: config.terminal.scrollback_lines, ..TermConfig::default() };

        // Create the alacritty_terminal Term.  It needs a `Dimensions`
        // implementor; we use `alacritty_terminal::event::WindowSize` directly
        // because it already satisfies `Dimensions` for grid construction.
        let dimensions = TermDimensions { columns: size.columns, screen_lines: size.rows };
        let term = Term::new(term_config, &dimensions, event_proxy.clone());

        // Spawn the PTY.
        let pty = PtyHandle::spawn(config, size, waker)?;

        Ok(Self { term, pty, event_proxy, parser: Processor::new(), title: String::new(), size })
    }

    // ── Input / output ────────────────────────────────────────────────────────

    /// Drain available PTY output, update the terminal grid, and return
    /// a [`ProcessOutcome`] summarizing noteworthy events that fired.
    ///
    /// Should be called from the main thread whenever the render loop
    /// wakes up.  Processes all bytes currently in the channel, then
    /// drains [`TerminalEvent`]s produced by the parser:
    ///
    /// - Title events update the cached title (exposed via [`Terminal::title`]).
    /// - `Exit` events populate [`ProcessOutcome::child_exit`] so the
    ///   caller can decide whether to close or freeze the window.
    /// - `Bell`, `Wakeup`, and `PtyWrite` are currently ignored (they
    ///   can be surfaced here when the app needs them — wire into
    ///   `ProcessOutcome`).
    pub fn process_input(&mut self) -> ProcessOutcome {
        let mut outcome = ProcessOutcome::default();

        // Drain all pending byte chunks from the reader thread.
        while let Ok(chunk) = self.pty.rx.try_recv() {
            self.parser.advance(&mut self.term, &chunk);
            outcome.grid_maybe_changed = true;
        }

        // Drain terminal events and update our cached title.  Exit
        // events are returned via `outcome` so the caller decides
        // close-vs-freeze policy.  Multiple Exit events in one call
        // collapse to the last one seen — a shell can only exit once.
        for event in self.event_proxy.drain() {
            match event {
                TerminalEvent::TitleChanged(t) => self.title = t,
                TerminalEvent::TitleReset => self.title.clear(),
                TerminalEvent::Exit(status) => outcome.child_exit = Some(status),
                // Bell / Wakeup / PtyWrite — not yet plumbed.  Dropping
                // them here matches previous behaviour.
                _ => {}
            }
        }

        // Poll the PTY reader's exit channel.  When the child shell
        // exits, the reader thread sees EOF on the PTY master, reaps
        // the child with `waitpid`, and pushes the status here.  We
        // don't run `alacritty_terminal`'s own event loop, so without
        // this bridge no `ChildExit` event would ever fire and the
        // window would stay open after the user typed `exit`.
        //
        // `try_recv` is non-blocking; a populated event from the event
        // proxy above (unlikely in practice) takes precedence via the
        // `is_none` guard so we don't clobber a real `ChildExit`
        // status with our waitpid-derived one.
        if outcome.child_exit.is_none() {
            if let Ok(status) = self.pty.exit_rx.try_recv() {
                outcome.child_exit = Some(status);
            }
        }

        outcome
    }

    /// Feed `data` directly to the VTE parser without writing to the PTY.
    ///
    /// Use this when the terminal emulator itself — not the child shell
    /// — is the source of bytes to display.  Current callers:
    ///
    /// - Exit banners ("[shell exited — press any key to close]")
    /// - Future: inline preedit display for IME composition
    ///
    /// The input is subject to the same ANSI/VTE parsing as PTY output,
    /// so colours (SGR sequences) and cursor-motion escapes work.
    pub fn inject_local(&mut self, data: &[u8]) {
        self.parser.advance(&mut self.term, data);
    }

    /// Send keyboard / paste `data` to the PTY.
    ///
    /// Also snaps the display back to the live area — matches xterm /
    /// iTerm2 / Terminal.app, where any user input into the shell returns
    /// the viewport to the cursor so the user can see what they're typing.
    pub fn write_to_pty(&mut self, data: &[u8]) -> Result<(), TerminalError> {
        self.term.scroll_display(Scroll::Bottom);
        self.pty.write(data)
    }

    /// Send `text` to the PTY as a paste, with clipboard-injection safety
    /// filtering.
    ///
    /// Filters applied unconditionally:
    ///
    /// - Bracketed-paste markers (`\x1b[200~` and `\x1b[201~`) are
    ///   stripped.  A clipboard payload containing the end marker
    ///   could otherwise escape the bracketed-paste wrap and smuggle
    ///   keystrokes into the shell — the canonical paste-injection
    ///   attack.
    /// - `\r\n` and lone `\r` are normalized to `\n` so pastes from
    ///   Windows / classic-Mac applications behave consistently and
    ///   a stray `\r` cannot act as "press Enter" in a non-bracketed
    ///   shell.
    ///
    /// Additional filtering when the shell has *not* enabled bracketed
    /// paste:
    ///
    /// - Any trailing newline is stripped.  Without bracketed paste
    ///   the shell reads each byte as a keystroke, so a trailing `\n`
    ///   would auto-execute the last pasted line before the user can
    ///   review it.  Embedded newlines are preserved for legitimate
    ///   multi-line pastes (heredocs, SQL, etc.).
    ///
    /// When bracketed paste IS active the filtered payload is wrapped
    /// in `\x1b[200~ … \x1b[201~` so readline treats it as a single
    /// edit (one undo step, history-expansion disabled, etc.).
    ///
    /// Prefer this over [`Self::write_to_pty`] for any byte stream
    /// that originated outside the user's physical keyboard — the
    /// system clipboard, X11-style middle-click primary selection,
    /// drag-and-drop, or any future shell-integrated paste command.
    pub fn paste(&mut self, text: &str) -> Result<(), TerminalError> {
        let bracketed = self.bracketed_paste();
        let filtered = crate::paste::filter(text, bracketed);

        if bracketed {
            // Wrap the sanitized payload in DECSET 2004 markers so
            // readline handles it as one edit.  `filter` has removed
            // any embedded markers, so the open/close bracket cannot
            // be escaped from inside.
            let mut payload = Vec::with_capacity(filtered.len() + BRACKETED_PASTE_WRAP_OVERHEAD);
            payload.extend_from_slice(BRACKETED_PASTE_START);
            payload.extend_from_slice(filtered.as_bytes());
            payload.extend_from_slice(BRACKETED_PASTE_END);
            self.write_to_pty(&payload)
        } else {
            self.write_to_pty(filtered.as_bytes())
        }
    }

    // ── Resize ────────────────────────────────────────────────────────────────

    /// Resize both the terminal grid and the PTY to `size`.
    pub fn resize(&mut self, size: TerminalSize) {
        if size == self.size {
            return;
        }
        self.size = size;

        // Resize the terminal grid.
        let dimensions = TermDimensions { columns: size.columns, screen_lines: size.rows };
        self.term.resize(dimensions);

        // Resize the PTY (send TIOCSWINSZ).
        // We do this by writing directly to the PTY file via ioctl; the
        // alacritty_terminal `OnResize` trait is implemented on `Pty` but we
        // don't have direct access to it after handing it to the reader
        // thread.  We replicate the resize via the underlying file descriptor
        // using libc instead.
        let window_size = size.to_window_size();
        if let Err(e) = resize_pty_fd(&self.pty, window_size) {
            log::warn!("PTY resize ioctl failed: {e}");
        }
    }

    // ── Grid access ───────────────────────────────────────────────────────────

    /// Read-only access to the terminal grid.
    ///
    /// The renderer calls this each frame to iterate over cells.
    pub fn grid(&self) -> &Grid<Cell> {
        self.term.grid()
    }

    // ── Title ─────────────────────────────────────────────────────────────────

    /// The current terminal title as set by OSC 0/2 sequences.
    ///
    /// Returns an empty string if no title has been set.
    pub fn title(&self) -> &str {
        &self.title
    }

    // ── Accessors for terminal mode / cursor ──────────────────────────────────

    /// The current terminal size.
    pub fn size(&self) -> TerminalSize {
        self.size
    }

    /// Whether the shell has enabled bracketed-paste mode via `DECSET 2004`.
    ///
    /// When true, pastes should be wrapped in `\x1b[200~ ... \x1b[201~` so
    /// readline sees the whole paste as one logical operation (relevant for
    /// undo and for shells that disable history expansion on pastes).
    pub fn bracketed_paste(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Whether the shell has enabled DECSET 1 (DECCKM) — application
    /// cursor mode.  When true, cursor / navigation keys must send SS3
    /// sequences (ESC O *) instead of CSI (ESC [ *).  Set by vim, less,
    /// tmux, readline's `cursor-keys-mode-application` option.
    pub fn cursor_app_mode(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    /// Mouse-reporting protocol currently negotiated with the shell.
    ///
    /// Read-only snapshot of the relevant DECSET flags.  Callers use
    /// this to decide whether a mouse event should be forwarded to the
    /// PTY as an escape sequence (when the running program has asked
    /// for mouse input — vim, tmux, fzf, less, tig, …) or consumed
    /// locally for selection / scrollback.
    pub fn mouse_protocol(&self) -> MouseProtocol {
        use alacritty_terminal::term::TermMode;
        let m = self.term.mode();
        MouseProtocol {
            report_click: m.contains(TermMode::MOUSE_REPORT_CLICK),
            report_drag: m.contains(TermMode::MOUSE_DRAG),
            report_motion: m.contains(TermMode::MOUSE_MOTION),
            sgr: m.contains(TermMode::SGR_MOUSE),
        }
    }

    /// Number of columns in the grid.
    pub fn columns(&self) -> usize {
        self.term.grid().columns()
    }

    /// Number of visible screen lines.
    pub fn screen_lines(&self) -> usize {
        self.term.grid().screen_lines()
    }

    // ── Scrollback ────────────────────────────────────────────────────────────

    /// Scroll the viewport up by `lines` lines (shows older content).
    pub fn scroll_up(&mut self, lines: usize) {
        self.term.scroll_display(Scroll::Delta(lines as i32));
    }

    /// Scroll the viewport down by `lines` lines (shows newer content).
    pub fn scroll_down(&mut self, lines: usize) {
        self.term.scroll_display(Scroll::Delta(-(lines as i32)));
    }

    // ── Cursor shape ──────────────────────────────────────────────────────────

    /// The current cursor shape as reported by the terminal state machine.
    pub fn cursor_shape(&self) -> CursorShape {
        self.term.cursor_style().shape
    }

    // ── Selection ─────────────────────────────────────────────────────────────

    /// Start a new character-level text selection at the given grid point.
    pub fn start_selection(&mut self, point: Point, side: Side) {
        self.term.selection = Some(Selection::new(SelectionType::Simple, point, side));
    }

    /// Extend the current selection to `point`.  No-op if no selection is active.
    pub fn update_selection(&mut self, point: Point, side: Side) {
        if let Some(sel) = self.term.selection.as_mut() {
            sel.update(point, side);
        }
    }

    /// Clear the current selection.
    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// Get the selected text as a `String`, or `None` if there is no (non-empty) selection.
    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string()
    }

    /// Get the current selection range for rendering, if any.
    pub fn selection_range(&self) -> Option<alacritty_terminal::selection::SelectionRange> {
        self.term.selection.as_ref().and_then(|s| s.to_range(&self.term))
    }

    // ── History ───────────────────────────────────────────────────────────────

    /// Clear the scrollback buffer and the visible screen.
    ///
    /// Matches iTerm2's Cmd+K: removes all scrollback history AND sends
    /// Ctrl+L (form feed) so the shell clears the visible viewport and
    /// redraws its prompt.  Clearing scrollback alone would be invisible
    /// to the user since the scrollback isn't rendered.
    pub fn clear_history(&mut self) {
        self.term.grid_mut().clear_history();
        let _ = self.pty.write(b"\x0c");
    }

    // ── Select all ───────────────────────────────────────────────────────────

    /// Create a selection that covers the entire scrollback + visible viewport.
    pub fn select_all(&mut self) {
        let start =
            Point::new(self.term.grid().topmost_line(), alacritty_terminal::index::Column(0));
        let end = Point::new(self.term.grid().bottommost_line(), self.term.grid().last_column());
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        self.term.selection = Some(selection);
    }
}

// ── Re-exports for callers ────────────────────────────────────────────────────

/// Re-export `Column` for constructing grid points.
pub use alacritty_terminal::index::Column as GridColumn;
/// Re-export `Line` for constructing grid points.
pub use alacritty_terminal::index::Line as GridLine;
/// Re-export `Point` so callers don't need to depend on `alacritty_terminal` directly.
pub use alacritty_terminal::index::Point as GridPoint;
/// Re-export `Side` so callers don't need to depend on `alacritty_terminal` directly.
pub use alacritty_terminal::index::Side as GridSide;

// ── Dimensions adapter ────────────────────────────────────────────────────────

/// A minimal `Dimensions` implementor used to construct/resize `Term`.
struct TermDimensions {
    columns: usize,
    screen_lines: usize,
}

impl alacritty_terminal::grid::Dimensions for TermDimensions {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

// ── PTY resize helper ─────────────────────────────────────────────────────────

/// Issue `TIOCSWINSZ` on the PTY master fd.
///
/// We replicate the resize logic from `alacritty_terminal::tty::Pty`'s
/// `OnResize` implementation, because after handing the `Pty` to the reader
/// thread we only have a cloned `File` (the writer) available.
/// Build a `libc::winsize` from a `WindowSize`, saturating the pixel fields at
/// `u16::MAX`.
///
/// The pixel fields (`ws_xpixel`, `ws_ypixel`) are informational: only
/// sixel-like protocols read them.  The products `col × cell_width` and
/// `row × cell_height` can easily exceed 65535 on 4K+ displays with small
/// fonts (e.g. 800 cols × 100 px = 80 000).  Silent wrapping would hand pixel-
/// aware apps a completely wrong value; saturation gives at least a lower bound
/// that such apps can clamp against.
fn winsize_from_window_size(window_size: WindowSize) -> libc::winsize {
    let ws_row = window_size.num_lines as libc::c_ushort;
    let ws_col = window_size.num_cols as libc::c_ushort;

    let xpixel_u32 = (window_size.num_cols as u32) * (window_size.cell_width as u32);
    let ypixel_u32 = (window_size.num_lines as u32) * (window_size.cell_height as u32);

    let ws_xpixel = xpixel_u32.min(u32::from(u16::MAX)) as libc::c_ushort;
    let ws_ypixel = ypixel_u32.min(u32::from(u16::MAX)) as libc::c_ushort;

    libc::winsize { ws_row, ws_col, ws_xpixel, ws_ypixel }
}

fn resize_pty_fd(pty: &PtyHandle, window_size: WindowSize) -> std::io::Result<()> {
    let winsize = winsize_from_window_size(window_size);

    let fd = pty.writer_fd();
    let res = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &winsize as *const _) };
    if res == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// No-op waker for tests that don't need to coordinate with an
    /// event loop.  The PTY reader thread will call it when bytes
    /// arrive; discarding the signal is safe because the test isn't
    /// observing PTY output via the event loop.
    fn noop_waker() -> crate::PtyWaker {
        std::sync::Arc::new(|| {})
    }

    #[test]
    fn term_dimensions_satisfies_trait() {
        let d = TermDimensions { columns: 80, screen_lines: 24 };
        assert_eq!(d.columns(), 80);
        assert_eq!(d.screen_lines(), 24);
        assert_eq!(d.total_lines(), 24);
    }

    #[test]
    fn new_rejects_zero_size() {
        let config = Config::default();
        let result = Terminal::new(
            &config,
            TerminalSize { columns: 0, rows: 24, cell_width: 8, cell_height: 16 },
            noop_waker(),
        );
        assert!(matches!(result, Err(TerminalError::InvalidSize { .. })));
    }

    #[test]
    fn terminal_spawns_and_has_grid() {
        let config = Config::default();
        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size, noop_waker()).expect("terminal should spawn");

        // Grid should have the requested dimensions.
        assert_eq!(term.columns(), 80);
        assert_eq!(term.screen_lines(), 24);

        // Title starts empty.
        assert!(term.title().is_empty());

        // Process input should not panic even with no PTY output yet.
        term.process_input();
    }

    #[test]
    fn terminal_resize_updates_dimensions() {
        let config = Config::default();
        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size, noop_waker()).expect("terminal should spawn");

        let new_size = TerminalSize { columns: 120, rows: 40, cell_width: 8, cell_height: 16 };
        term.resize(new_size);
        assert_eq!(term.columns(), 120);
        assert_eq!(term.screen_lines(), 40);
    }

    #[test]
    fn terminal_write_to_pty_succeeds() {
        let config = Config::default();
        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size, noop_waker()).expect("terminal should spawn");

        // Writing bytes to the PTY should not fail.
        term.write_to_pty(b"echo hello\n").expect("PTY write should succeed");
    }

    #[test]
    fn terminal_clear_history_does_not_panic() {
        let config = Config::default();
        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size, noop_waker()).expect("terminal should spawn");

        // Clear on a fresh terminal (no history yet) should be a no-op that
        // doesn't panic.
        term.clear_history();
    }

    #[test]
    fn terminal_paste_plain_text_succeeds() {
        // Smoke test: happy-path paste doesn't panic and doesn't error.
        // Filter-level semantics are covered exhaustively by
        // `paste::tests` — here we just verify the plumbing from
        // `Terminal::paste` through the filter into the PTY works.
        let config = Config::default();
        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size, noop_waker()).expect("terminal should spawn");

        term.paste("echo hello\n").expect("plain paste should succeed");
    }

    #[test]
    fn terminal_paste_tolerates_injection_attempt() {
        // Payload contains the bracketed-paste end marker — the filter
        // must strip it so no shell-injection vector survives.  We
        // can't easily read the PTY back to assert the exact bytes,
        // but we can verify the call itself doesn't panic or error
        // and that the filter module did its job (covered by its own
        // tests).  This test exists as a regression guard: if someone
        // ever replaces `paste` with a naive write that skips the
        // filter, this path still runs but `paste::tests` would have
        // already failed at compile/test time.
        let config = Config::default();
        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size, noop_waker()).expect("terminal should spawn");

        let malicious = "safe\x1b[201~; rm -rf /";
        term.paste(malicious).expect("filtered paste should succeed");
    }

    #[test]
    fn terminal_paste_empty_string_succeeds() {
        // Edge case: user pastes nothing (empty clipboard).  Should
        // be a no-op-like write that the PTY tolerates.  Wrapped
        // bracketed-paste markers around an empty payload are still
        // a valid (if pointless) DECSET 2004 exchange.
        let config = Config::default();
        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size, noop_waker()).expect("terminal should spawn");

        term.paste("").expect("empty paste should succeed");
    }

    #[test]
    fn process_input_no_input_reports_clean_outcome() {
        // Freshly-spawned terminal with no shell output yet: the
        // outcome should report no grid change and no exit.  This
        // underpins the animation-fast-path — if this fired
        // spuriously we'd do a full render every frame forever.
        let mut config = Config::default();
        config.shell.program = "/bin/sh".into();

        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size, noop_waker()).expect("terminal should spawn");

        // Immediately, before the shell has produced anything: nothing
        // to drain.
        let outcome = term.process_input();
        assert!(!outcome.grid_maybe_changed);
        assert!(outcome.child_exit.is_none());
    }

    #[test]
    fn process_input_flags_grid_change_after_shell_output() {
        // After the shell prints its prompt (or any output), a
        // subsequent `process_input` call should report
        // `grid_maybe_changed == true` exactly once — successive
        // calls with no new output report `false` again.
        let mut config = Config::default();
        config.shell.program = "/bin/sh".into();

        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size, noop_waker()).expect("terminal should spawn");

        // Nudge the shell to produce deterministic output.  `echo hi`
        // + newline yields at least an echo back and an "hi" line on
        // most sh implementations.
        term.write_to_pty(b"echo hi\n").expect("write should succeed");

        // Poll until we see the grid change flag trip.  3 s is
        // generous; /bin/sh responds in milliseconds.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut saw_change = false;
        while std::time::Instant::now() < deadline {
            let outcome = term.process_input();
            if outcome.grid_maybe_changed {
                saw_change = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(saw_change, "grid_maybe_changed should trip after shell output");

        // Drain any remaining bytes, then assert steady state reports false.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = term.process_input(); // drain leftovers
        let outcome = term.process_input();
        assert!(
            !outcome.grid_maybe_changed,
            "no new bytes → grid_maybe_changed should be false"
        );
    }

    #[test]
    fn process_input_reports_child_exit_after_shell_exits() {
        // Regression test for the "typing `exit` leaves the window
        // open" bug: when the child shell exits, the PTY master
        // observes EOF and the reader thread must reap the child so
        // the next `process_input` call surfaces a populated
        // `child_exit` in the outcome.
        //
        // Uses `/bin/sh` explicitly rather than `Config::default()`'s
        // `$SHELL` so the test is deterministic across developer
        // machines where `$SHELL` may point at zsh/fish/nushell with
        // different startup-file side effects.
        let mut config = Config::default();
        config.shell.program = "/bin/sh".into();

        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size, noop_waker()).expect("terminal should spawn");

        // Give the shell a moment to start, then ask it to exit.
        // `exit 0\n` is understood by every POSIX sh.
        std::thread::sleep(std::time::Duration::from_millis(100));
        term.write_to_pty(b"exit 0\n").expect("write should succeed");

        // Poll `process_input` until we see the exit or time out.
        // 3 seconds is generous — `sh` exits in milliseconds.  The
        // loop keeps the grid up to date so we don't miss a fast
        // exit that lands between sleeps.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut observed = None;
        while std::time::Instant::now() < deadline {
            let outcome = term.process_input();
            if let Some(status) = outcome.child_exit {
                observed = Some(status);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let status = observed.expect("child_exit should be populated after shell exits");
        // `exit 0` → a real ExitStatus with code 0.
        let status = status.expect("status should carry a real waitpid result, not None");
        assert!(status.success(), "expected success, got {status:?}");
    }

    #[test]
    fn terminal_select_all_populates_selection() {
        let config = Config::default();
        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size, noop_waker()).expect("terminal should spawn");

        // Before select_all, no selection text.
        assert!(term.selection_text().is_none());

        term.select_all();

        // After select_all, selection_range should exist (even if the grid is
        // empty — an empty terminal still has an area to select).
        assert!(term.selection_range().is_some());
    }

    // ── winsize_from_window_size ──────────────────────────────────────────────

    fn make_ws(num_cols: u16, num_lines: u16, cell_width: u16, cell_height: u16) -> WindowSize {
        WindowSize { num_cols, num_lines, cell_width, cell_height }
    }

    #[test]
    fn winsize_normal_case_exact() {
        // 80 cols × 8 px wide, 24 rows × 16 px tall → 640 × 384, no overflow.
        let ws = winsize_from_window_size(make_ws(80, 24, 8, 16));
        assert_eq!(ws.ws_col, 80);
        assert_eq!(ws.ws_row, 24);
        assert_eq!(ws.ws_xpixel, 640);
        assert_eq!(ws.ws_ypixel, 384);
    }

    #[test]
    fn winsize_saturates_xpixel() {
        // 800 cols × 100 px = 80 000 > 65 535 → must saturate to u16::MAX.
        let ws = winsize_from_window_size(make_ws(800, 24, 100, 16));
        assert_eq!(ws.ws_xpixel, u16::MAX);
        // Row count and col count are unaffected.
        assert_eq!(ws.ws_col, 800);
        assert_eq!(ws.ws_row, 24);
    }

    #[test]
    fn winsize_saturates_ypixel() {
        // 24 rows × 3000 px = 72 000 > 65 535 → must saturate to u16::MAX.
        let ws = winsize_from_window_size(make_ws(80, 24, 8, 3000));
        assert_eq!(ws.ws_ypixel, u16::MAX);
        assert_eq!(ws.ws_row, 24);
    }

    #[test]
    fn winsize_zero_size_does_not_panic() {
        // Shouldn't happen in practice, but must not overflow or panic.
        let ws = winsize_from_window_size(make_ws(0, 0, 0, 0));
        assert_eq!(ws.ws_col, 0);
        assert_eq!(ws.ws_row, 0);
        assert_eq!(ws.ws_xpixel, 0);
        assert_eq!(ws.ws_ypixel, 0);
    }
}
