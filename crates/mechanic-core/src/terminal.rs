//! Terminal state wrapper.

use std::time::{Duration, Instant};

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

/// Result of one [`Terminal::process_input`] call.
#[derive(Debug, Clone, Default)]
pub struct ProcessOutcome {
    /// Exit observed in this call: `None` means no event, `Some(None)` means unknown status.
    /// `Some(Some(status))` carries the child status; exit is delivered once.
    pub child_exit: Option<Option<std::process::ExitStatus>>,

    /// Parser received PTY bytes; the visible grid may need rebuilding.
    pub grid_maybe_changed: bool,
    /// Fatal asynchronous transport failure, delivered once.
    pub io_error: Option<String>,
    /// A parsing budget was reached; schedule another call even without a PTY wake.
    /// This is conservative and may be true when the last chunk emptied the queue.
    pub more_output: bool,
}

const PARSE_TIME_BUDGET: Duration = Duration::from_millis(4);
const PARSE_BYTE_BUDGET: usize = 4 * 1024 * 1024;
const PARSE_CHUNK_BUDGET: usize = 64;

/// Snapshot of the DECSET mouse-reporting flags the running program has enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MouseProtocol {
    /// DECSET 1000 — press/release events.
    pub report_click: bool,
    /// DECSET 1002 — press/release + motion while a button is held.
    pub report_drag: bool,
    /// DECSET 1003 — all motion events.
    pub report_motion: bool,
    /// DECSET 1006 selects SGR encoding; otherwise use legacy mouse encoding.
    pub sgr: bool,
}

impl MouseProtocol {
    /// Whether any mouse reporting mode is enabled.
    pub fn is_tracking(&self) -> bool {
        self.report_click || self.report_drag || self.report_motion
    }
}

/// DECSET 2004 paste markers.
const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";

const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

const BRACKETED_PASTE_WRAP_OVERHEAD: usize =
    BRACKETED_PASTE_START.len() + BRACKETED_PASTE_END.len();

/// Terminal parser and grid, fed by output from a background PTY reader.
/// Process input and access the grid on the same thread.
pub struct Terminal {
    term: Term<EventProxy>,
    /// Writer and output channels; the reader thread owns the PTY.
    pty: PtyHandle,
    event_proxy: EventProxy,
    parser: Processor,
    /// Cached OSC title; empty means the application supplies its default.
    title: String,
    size: TerminalSize,
    pending_exit: Option<Option<std::process::ExitStatus>>,
    exit_delivered: bool,
    pending_error: Option<String>,
    output_finished: bool,
}

impl Terminal {
    /// Create a new terminal with a PTY of the given size.
    pub fn new(
        config: &Config,
        size: TerminalSize,
        waker: PtyWaker,
    ) -> Result<Self, TerminalError> {
        if size.columns == 0 || size.rows == 0 {
            return Err(TerminalError::InvalidSize { columns: size.columns, rows: size.rows });
        }

        let event_proxy = EventProxy::new();

        let term_config = TermConfig {
            scrolling_history: config.terminal.scrollback_lines,
            ..TermConfig::default()
        };

        let dimensions = TermDimensions { columns: size.columns, screen_lines: size.rows };
        let term = Term::new(term_config, &dimensions, event_proxy.clone());

        let pty = PtyHandle::spawn(config, size, waker)?;

        Ok(Self {
            term,
            pty,
            event_proxy,
            parser: Processor::new(),
            title: String::new(),
            size,
            pending_exit: None,
            exit_delivered: false,
            pending_error: None,
            output_finished: false,
        })
    }

    /// Drain output, update the grid/title, and send terminal protocol replies.
    pub fn process_input(&mut self) -> ProcessOutcome {
        self.process_input_with_budget(Instant::now(), PARSE_TIME_BUDGET, PARSE_BYTE_BUDGET)
    }

    // Check budgets between whole queue chunks so VTE state carries incomplete
    // UTF-8 and escape sequences naturally. Always parse one available chunk.
    fn process_input_with_budget(
        &mut self,
        started: Instant,
        time_budget: Duration,
        byte_budget: usize,
    ) -> ProcessOutcome {
        let mut outcome = ProcessOutcome::default();
        // Observe completion BEFORE checking for an empty output queue. An
        // empty observation made earlier cannot prove final output was parsed.
        let output_done = self.pty.output_done();
        // The worker sends its status before publishing output_done. Read the
        // flag first so completion cannot hide a concurrently arriving status.
        let pty_exit = self.pty.exit_rx.try_recv().ok();
        self.output_finished |= output_done || pty_exit.is_some();
        if !self.exit_delivered
            && let Some(status) = pty_exit
        {
            self.pending_exit = Some(status);
        }
        if let Some(error) = self.pty.take_failure() {
            self.pending_error = Some(error);
        }
        self.drain_parser_events();
        let mut bytes = 0;
        let mut chunks = 0;
        let mut empty = false;

        loop {
            if chunks > 0
                && (started.elapsed() >= time_budget
                    || bytes >= byte_budget
                    || chunks >= PARSE_CHUNK_BUDGET)
            {
                outcome.more_output = true;
                break;
            }
            let Ok(chunk) = self.pty.rx.try_recv() else {
                empty = true;
                break;
            };
            bytes += chunk.len();
            chunks += 1;
            self.parser.advance(&mut self.term, &chunk);
            outcome.grid_maybe_changed = true;
            // Include protocol replies and title processing in the time budget.
            self.drain_parser_events();
        }
        if outcome.grid_maybe_changed {
            self.pty.output_drained();
        }
        // output_drained or a protocol reply can itself fail to notify the
        // worker. Retain that failure before deciding whether to report exit.
        if let Some(error) = self.pty.take_failure() {
            self.pending_error = Some(error);
        }

        if empty {
            // Only an observed PTY exit or worker completion guarantees that
            // another producer cannot race this empty queue observation.
            if self.output_finished {
                outcome.child_exit = self.pending_exit.take();
                self.exit_delivered |= outcome.child_exit.is_some();
                outcome.io_error = self.pending_error.take();
            }
        }

        outcome
    }

    fn drain_parser_events(&mut self) {
        for event in self.event_proxy.drain() {
            match event {
                TerminalEvent::TitleChanged(t) => self.title = t,
                TerminalEvent::TitleReset => self.title.clear(),
                TerminalEvent::Exit(status) if !self.exit_delivered => {
                    self.pending_exit.get_or_insert(status);
                }
                TerminalEvent::PtyWrite(bytes) => {
                    // Protocol replies must not move a scrolled viewport.
                    if let Err(error) = self.pty.write(&bytes) {
                        log::warn!("could not write terminal reply: {error}");
                    }
                }
                _ => {}
            }
        }
    }

    /// Feed `data` directly to the VTE parser without writing to the PTY.
    pub fn inject_local(&mut self, data: &[u8]) {
        self.parser.advance(&mut self.term, data);
    }

    /// Send input and return the scrollback viewport to the live screen.
    pub fn write_to_pty(&mut self, data: &[u8]) -> Result<(), TerminalError> {
        self.term.scroll_display(Scroll::Bottom);
        self.pty.write(data)
    }

    /// Filter clipboard text and wrap it when DECSET 2004 is enabled.
    /// See [`crate::paste::filter`] for the filtering contract.
    pub fn paste(&mut self, text: &str) -> Result<(), TerminalError> {
        let bracketed = self.bracketed_paste();
        let filtered = crate::paste::filter(text, bracketed);

        if bracketed {
            let mut payload = Vec::with_capacity(filtered.len() + BRACKETED_PASTE_WRAP_OVERHEAD);
            payload.extend_from_slice(BRACKETED_PASTE_START);
            payload.extend_from_slice(filtered.as_bytes());
            payload.extend_from_slice(BRACKETED_PASTE_END);
            self.write_to_pty(&payload)
        } else {
            self.write_to_pty(filtered.as_bytes())
        }
    }

    /// Resize both the terminal grid and the PTY to `size`.
    pub fn resize(&mut self, size: TerminalSize) {
        if size == self.size {
            return;
        }
        self.size = size;

        let dimensions = TermDimensions { columns: size.columns, screen_lines: size.rows };
        self.term.resize(dimensions);

        let window_size = size.to_window_size();
        if let Err(e) = resize_pty_fd(&self.pty, window_size) {
            log::warn!("PTY resize ioctl failed: {e}");
        }
    }

    pub fn grid(&self) -> &Grid<Cell> {
        self.term.grid()
    }

    /// Visible content with the effective cursor visibility and wide-cell position.
    pub fn renderable_content(&self) -> alacritty_terminal::term::RenderableContent<'_> {
        self.term.renderable_content()
    }

    /// The current terminal title as set by OSC 0/2 sequences.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The current terminal size.
    pub fn size(&self) -> TerminalSize {
        self.size
    }

    /// Whether the shell has enabled bracketed-paste mode via `DECSET 2004`.
    pub fn bracketed_paste(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// DECSET 1 (DECCKM): arrows and Home/End use SS3 rather than CSI.
    pub fn cursor_app_mode(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    /// Mouse-reporting protocol currently negotiated with the shell.
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

    pub fn columns(&self) -> usize {
        self.term.grid().columns()
    }

    pub fn screen_lines(&self) -> usize {
        self.term.grid().screen_lines()
    }

    /// Scroll the viewport up by `lines` lines (shows older content).
    pub fn scroll_up(&mut self, lines: usize) {
        self.term.scroll_display(Scroll::Delta(lines as i32));
    }

    /// Scroll the viewport down by `lines` lines (shows newer content).
    pub fn scroll_down(&mut self, lines: usize) {
        self.term.scroll_display(Scroll::Delta(-(lines as i32)));
    }

    /// The current cursor shape as reported by the terminal state machine.
    pub fn cursor_shape(&self) -> CursorShape {
        self.term.cursor_style().shape
    }

    /// Start a character selection in live-grid coordinates; scrollback lines are negative.
    pub fn start_selection(&mut self, point: Point, side: Side) {
        self.term.selection = Some(Selection::new(SelectionType::Simple, point, side));
    }

    /// Extend the current selection to `point`.  No-op if no selection is active.
    pub fn update_selection(&mut self, point: Point, side: Side) {
        if let Some(sel) = self.term.selection.as_mut() {
            sel.update(point, side);
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    /// Selected text, or `None` for an absent or empty selection.
    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string()
    }

    /// Get the current selection range for rendering, if any.
    pub fn selection_range(&self) -> Option<alacritty_terminal::selection::SelectionRange> {
        self.term.selection.as_ref().and_then(|s| s.to_range(&self.term))
    }

    /// Clear scrollback and send Ctrl+L to ask the foreground program to redraw.
    pub fn clear_history(&mut self) {
        self.term.grid_mut().clear_history();
        let _ = self.pty.write(b"\x0c");
    }

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

/// Zero-based grid column.
pub use alacritty_terminal::index::Column as GridColumn;
/// Live-screen row; negative values address scrollback.
pub use alacritty_terminal::index::Line as GridLine;
/// Grid point in live-screen coordinates.
pub use alacritty_terminal::index::Point as GridPoint;
/// Left or right cell half for selection boundaries.
pub use alacritty_terminal::index::Side as GridSide;

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

/// Issue `TIOCSWINSZ` on the PTY master fd.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn buffered_terminal() -> (Terminal, crate::pty::TestPtyPeer) {
        let (pty, peer) = PtyHandle::test_pair(noop_waker());
        let size = TerminalSize::default();
        let dimensions = TermDimensions { columns: size.columns, screen_lines: size.rows };
        let event_proxy = EventProxy::new();
        let term = Term::new(TermConfig::default(), &dimensions, event_proxy.clone());
        (
            Terminal {
                term,
                pty,
                event_proxy,
                parser: Processor::new(),
                title: String::new(),
                size,
                pending_exit: None,
                exit_delivered: false,
                pending_error: None,
                output_finished: false,
            },
            peer,
        )
    }

    fn parse_one_chunk(terminal: &mut Terminal) -> ProcessOutcome {
        terminal.process_input_with_budget(Instant::now(), Duration::ZERO, PARSE_BYTE_BUDGET)
    }

    #[test]
    fn buffered_output_continues_and_preserves_split_sequences_before_exit() {
        use std::os::unix::process::ExitStatusExt as _;

        let (mut terminal, peer) = buffered_terminal();
        // The OSC sequence crosses one queue chunk, and the UTF-8 scalar
        // crosses the next. No producer wakes occur while parsing this buffer.
        let mut payload = vec![b' '; 64 * 1024 - 2];
        payload.extend_from_slice(b"\x1b]");
        payload.extend_from_slice(b"2;final-title\x07");
        payload.resize(128 * 1024 - 1, b' ');
        payload.push(0xe2);
        payload.extend_from_slice(b"\x82\xac\x1b]2;complete\x07\x1b[H\x1b[6n");
        peer.send(payload);
        let status = std::process::ExitStatus::from_raw(0);
        peer.exit(Some(status));

        let first = parse_one_chunk(&mut terminal);
        assert!(first.grid_maybe_changed && first.more_output);
        assert!(first.child_exit.is_none());
        assert_eq!(terminal.title(), "");

        let second = parse_one_chunk(&mut terminal);
        assert!(second.more_output && second.child_exit.is_none());
        assert_eq!(terminal.title(), "final-title");

        let third = parse_one_chunk(&mut terminal);
        assert!(third.more_output && third.child_exit.is_none());
        assert_eq!(terminal.title(), "complete");
        assert!(terminal.term.grid().display_iter().any(|cell| cell.cell.c == '€'));
        assert_eq!(peer.reply(), b"\x1b[1;1R");

        let final_call = parse_one_chunk(&mut terminal);
        assert_eq!(final_call.child_exit, Some(Some(status)));
        assert!(!final_call.more_output);
        assert!(terminal.process_input().child_exit.is_none());
    }

    #[test]
    fn failure_waits_for_worker_completion_and_final_output() {
        let (mut terminal, peer) = buffered_terminal();
        peer.send(b"\x1b]2;before-failure\x07".to_vec());
        peer.fail();
        let first = terminal.process_input();
        assert_eq!(terminal.title(), "before-failure");
        assert!(first.io_error.is_none());
        assert!(!first.more_output);
        // Model a failure raised on the UI while a worker is still producing.
        peer.send(b"\x1b]2;final-output\x07".to_vec());
        peer.finish();
        let budgeted = parse_one_chunk(&mut terminal);
        assert!(budgeted.io_error.is_none() && budgeted.more_output);
        assert_eq!(terminal.title(), "final-output");
        let done = terminal.process_input();
        assert_eq!(done.io_error.as_deref(), Some("test transport failure"));
        assert!(terminal.process_input().io_error.is_none());
    }

    #[test]
    fn exit_publication_before_worker_completion_preserves_failure_precedence() {
        use std::os::unix::process::ExitStatusExt as _;

        let (mut terminal, peer) = buffered_terminal();
        peer.send(b"\x1b]2;final-output\x07".to_vec());
        peer.fail();
        let status = std::process::ExitStatus::from_raw(0);
        // This status already guarantees no more enqueues, though the worker
        // has not yet reached its output_done store and completion wake.
        peer.publish_exit(Some(status));
        assert!(!terminal.pty.output_done());
        let outcome = terminal.process_input();
        assert_eq!(terminal.title(), "final-output");
        assert_eq!(outcome.child_exit, Some(Some(status)));
        assert_eq!(outcome.io_error.as_deref(), Some("test transport failure"));
        let next = terminal.process_input();
        assert!(next.child_exit.is_none() && next.io_error.is_none());
    }

    #[test]
    fn parser_exit_is_retained_until_final_output_and_delivered_once() {
        use alacritty_terminal::event::{Event, EventListener as _};

        let (mut terminal, peer) = buffered_terminal();
        terminal.event_proxy.send_event(Event::Exit);
        let waiting = terminal.process_input();
        assert!(waiting.child_exit.is_none() && !waiting.more_output);
        peer.send(vec![b' '; 128 * 1024]);
        peer.finish();
        assert!(parse_one_chunk(&mut terminal).child_exit.is_none());
        assert!(parse_one_chunk(&mut terminal).child_exit.is_none());
        assert_eq!(terminal.process_input().child_exit, Some(None));
        terminal.event_proxy.send_event(Event::Exit);
        assert!(terminal.process_input().child_exit.is_none());
    }

    #[test]
    fn expired_time_budget_still_makes_one_chunk_of_progress() {
        let (mut terminal, peer) = buffered_terminal();
        peer.send(vec![b' '; 128 * 1024]);
        let expired = Instant::now() - Duration::from_secs(1);
        let first = terminal.process_input_with_budget(expired, PARSE_TIME_BUDGET, 0);
        assert!(first.grid_maybe_changed && first.more_output);
        assert_eq!(terminal.pty.rx.try_recv().unwrap().len(), 64 * 1024);
        assert!(terminal.pty.rx.try_recv().is_err());
    }

    #[test]
    fn hard_byte_budget_allows_four_mebibytes_and_requests_continuation() {
        let (mut terminal, peer) = buffered_terminal();
        peer.send(vec![0; PARSE_BYTE_BUDGET]);
        let outcome = terminal.process_input_with_budget(
            Instant::now(),
            Duration::from_secs(60),
            PARSE_BYTE_BUDGET,
        );
        assert!(outcome.grid_maybe_changed && outcome.more_output);
        assert!(terminal.pty.rx.try_recv().is_err());
        assert!(!terminal.process_input().more_output);
    }

    #[test]
    fn cursor_query_reply_reaches_child() {
        let mut config = Config::default();
        config.shell.program = "/bin/sh".into();
        let mut terminal = Terminal::new(&config, TerminalSize::default(), noop_waker()).unwrap();
        // Raw input lets dd consume the six-byte CPR without waiting for a newline.
        terminal.write_to_pty(b"stty raw -echo; printf '\\033[H\\033[6n'; reply=$(dd bs=1 count=6 2>/dev/null); if [ \"$reply\" = \"$(printf '\\033[1;1R')\" ]; then printf '\\033]2;reply-ok\\007'; fi; exit\n").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            terminal.process_input();
            if terminal.title() == "reply-ok" {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        terminal.write_to_pty(b"\x1b[1;1R").ok();
        panic!("child did not receive the cursor-position reply");
    }

    /// Ignore output notifications in tests that poll the terminal directly.
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

        assert_eq!(term.columns(), 80);
        assert_eq!(term.screen_lines(), 24);

        assert!(term.title().is_empty());

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

        term.write_to_pty(b"echo hello\n").expect("PTY write should succeed");
    }

    #[test]
    fn terminal_clear_history_does_not_panic() {
        let config = Config::default();
        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size, noop_waker()).expect("terminal should spawn");

        term.clear_history();
    }

    #[test]
    fn terminal_paste_plain_text_succeeds() {
        let config = Config::default();
        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size, noop_waker()).expect("terminal should spawn");

        term.paste("echo hello\n").expect("plain paste should succeed");
    }

    #[test]
    fn terminal_paste_tolerates_injection_attempt() {
        let config = Config::default();
        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size, noop_waker()).expect("terminal should spawn");

        let malicious = "safe\x1b[201~; rm -rf /";
        term.paste(malicious).expect("filtered paste should succeed");
    }

    #[test]
    fn terminal_paste_empty_string_succeeds() {
        let config = Config::default();
        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size, noop_waker()).expect("terminal should spawn");

        term.paste("").expect("empty paste should succeed");
    }

    #[test]
    fn process_input_no_input_reports_clean_outcome() {
        let mut config = Config::default();
        config.shell.program = "/bin/sh".into();

        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size, noop_waker()).expect("terminal should spawn");

        let outcome = term.process_input();
        assert!(!outcome.grid_maybe_changed);
        assert!(outcome.child_exit.is_none());
    }

    #[test]
    fn process_input_flags_grid_change_after_shell_output() {
        let mut config = Config::default();
        config.shell.program = "/bin/sh".into();

        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size, noop_waker()).expect("terminal should spawn");

        term.write_to_pty(b"echo hi\n").expect("write should succeed");

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

        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = term.process_input(); // drain leftovers
        let outcome = term.process_input();
        assert!(!outcome.grid_maybe_changed, "no new bytes → grid_maybe_changed should be false");
    }

    #[test]
    fn process_input_reports_child_exit_after_shell_exits() {
        let mut config = Config::default();
        config.shell.program = "/bin/sh".into();

        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size, noop_waker()).expect("terminal should spawn");

        std::thread::sleep(std::time::Duration::from_millis(100));
        term.write_to_pty(b"exit 0\n").expect("write should succeed");

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
        let status = status.expect("status should carry a real waitpid result, not None");
        assert!(status.success(), "expected success, got {status:?}");
    }

    #[test]
    fn terminal_select_all_populates_selection() {
        let config = Config::default();
        let size = TerminalSize { columns: 80, rows: 24, cell_width: 8, cell_height: 16 };
        let mut term = Terminal::new(&config, size, noop_waker()).expect("terminal should spawn");

        assert!(term.selection_text().is_none());

        term.select_all();

        assert!(term.selection_range().is_some());
    }

    fn make_ws(num_cols: u16, num_lines: u16, cell_width: u16, cell_height: u16) -> WindowSize {
        WindowSize { num_cols, num_lines, cell_width, cell_height }
    }

    #[test]
    fn winsize_normal_case_exact() {
        let ws = winsize_from_window_size(make_ws(80, 24, 8, 16));
        assert_eq!(ws.ws_col, 80);
        assert_eq!(ws.ws_row, 24);
        assert_eq!(ws.ws_xpixel, 640);
        assert_eq!(ws.ws_ypixel, 384);
    }

    #[test]
    fn winsize_saturates_xpixel() {
        let ws = winsize_from_window_size(make_ws(800, 24, 100, 16));
        assert_eq!(ws.ws_xpixel, u16::MAX);
        assert_eq!(ws.ws_col, 800);
        assert_eq!(ws.ws_row, 24);
    }

    #[test]
    fn winsize_saturates_ypixel() {
        let ws = winsize_from_window_size(make_ws(80, 24, 8, 3000));
        assert_eq!(ws.ws_ypixel, u16::MAX);
        assert_eq!(ws.ws_row, 24);
    }

    #[test]
    fn winsize_zero_size_does_not_panic() {
        let ws = winsize_from_window_size(make_ws(0, 0, 0, 0));
        assert_eq!(ws.ws_col, 0);
        assert_eq!(ws.ws_row, 0);
        assert_eq!(ws.ws_xpixel, 0);
        assert_eq!(ws.ws_ypixel, 0);
    }
}
