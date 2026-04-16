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

use crate::TerminalSize;
use crate::error::TerminalError;
use crate::event::{EventProxy, TerminalEvent};
use crate::pty::PtyHandle;

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
    pub fn new(config: &Config, size: TerminalSize) -> Result<Self, TerminalError> {
        if size.columns == 0 || size.rows == 0 {
            return Err(TerminalError::InvalidSize { columns: size.columns, rows: size.rows });
        }

        let event_proxy = EventProxy::new();

        // Build a `Term` config. We use defaults and can extend later.
        let term_config = TermConfig::default();

        // Create the alacritty_terminal Term.  It needs a `Dimensions`
        // implementor; we use `alacritty_terminal::event::WindowSize` directly
        // because it already satisfies `Dimensions` for grid construction.
        let dimensions = TermDimensions { columns: size.columns, screen_lines: size.rows };
        let term = Term::new(term_config, &dimensions, event_proxy.clone());

        // Spawn the PTY.
        let pty = PtyHandle::spawn(config, size)?;

        Ok(Self { term, pty, event_proxy, parser: Processor::new(), title: String::new(), size })
    }

    // ── Input / output ────────────────────────────────────────────────────────

    /// Drain available PTY output and update the terminal grid.
    ///
    /// Should be called from the main thread whenever the render loop wakes
    /// up.  Processes all bytes currently in the channel, then drains any
    /// [`TerminalEvent`]s produced (updating the title cache, etc.).
    pub fn process_input(&mut self) {
        // Drain all pending byte chunks from the reader thread.
        while let Ok(chunk) = self.pty.rx.try_recv() {
            self.parser.advance(&mut self.term, &chunk);
        }

        // Drain terminal events and update our cached title.
        for event in self.event_proxy.drain() {
            match event {
                TerminalEvent::TitleChanged(t) => self.title = t,
                TerminalEvent::TitleReset => self.title.clear(),
                // Other events (Bell, Wakeup, Exit, PtyWrite) are surfaced to
                // callers via `poll_events`; we don't need to act on them here.
                _ => {}
            }
        }
    }

    /// Collect and return all pending [`TerminalEvent`]s.
    ///
    /// This includes events that `process_input` did **not** consume
    /// internally (Bell, Wakeup, Exit, PtyWrite).  Call this after
    /// `process_input` each frame.
    pub fn poll_events(&self) -> Vec<TerminalEvent> {
        self.event_proxy.drain()
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
fn resize_pty_fd(pty: &PtyHandle, window_size: WindowSize) -> std::io::Result<()> {
    let ws_row = window_size.num_lines as libc::c_ushort;
    let ws_col = window_size.num_cols as libc::c_ushort;
    let ws_xpixel = ws_col * window_size.cell_width as libc::c_ushort;
    let ws_ypixel = ws_row * window_size.cell_height as libc::c_ushort;

    let winsize = libc::winsize { ws_row, ws_col, ws_xpixel, ws_ypixel };

    let fd = pty.writer_fd();
    let res = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &winsize as *const _) };
    if res == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
        );
        assert!(matches!(result, Err(TerminalError::InvalidSize { .. })));
    }

    #[test]
    fn terminal_spawns_and_has_grid() {
        let config = Config::default();
        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size).expect("terminal should spawn");

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
        let mut term = Terminal::new(&config, size).expect("terminal should spawn");

        let new_size = TerminalSize { columns: 120, rows: 40, cell_width: 8, cell_height: 16 };
        term.resize(new_size);
        assert_eq!(term.columns(), 120);
        assert_eq!(term.screen_lines(), 40);
    }

    #[test]
    fn terminal_write_to_pty_succeeds() {
        let config = Config::default();
        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size).expect("terminal should spawn");

        // Writing bytes to the PTY should not fail.
        term.write_to_pty(b"echo hello\n").expect("PTY write should succeed");
    }

    #[test]
    fn terminal_clear_history_does_not_panic() {
        let config = Config::default();
        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size).expect("terminal should spawn");

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
        let mut term = Terminal::new(&config, size).expect("terminal should spawn");

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
        let mut term = Terminal::new(&config, size).expect("terminal should spawn");

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
        let mut term = Terminal::new(&config, size).expect("terminal should spawn");

        term.paste("").expect("empty paste should succeed");
    }

    #[test]
    fn terminal_select_all_populates_selection() {
        let config = Config::default();
        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size).expect("terminal should spawn");

        // Before select_all, no selection text.
        assert!(term.selection_text().is_none());

        term.select_all();

        // After select_all, selection_range should exist (even if the grid is
        // empty — an empty terminal still has an area to select).
        assert!(term.selection_range().is_some());
    }
}
