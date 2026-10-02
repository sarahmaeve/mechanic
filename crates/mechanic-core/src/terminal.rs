//! Terminal state wrapper.

use std::time::{Duration, Instant};

use alacritty_terminal::Grid;
use alacritty_terminal::Term;
use alacritty_terminal::event::{Event as AlacrittyEvent, WindowSize};
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::vte::ansi::{CursorShape, Processor, Rgb};
use mechanic_config::{Config, Theme};

use crate::PtyWaker;
use crate::TerminalSize;
use crate::error::TerminalError;
use crate::event::{EventProxy, TerminalEvent};
use crate::pty::PtyHandle;
use crate::shell_state::{ShellIntegration, ShellPosition};

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
    /// Detect ST when its ESC and backslash arrive in separate PTY chunks.
    previous_byte_is_escape: bool,
    /// Conservative OSC query candidate, carried until the next terminator.
    question_mark_since_terminator: bool,
    /// Formatted replies for the current parser chunk, queued as one PTY write.
    reply_buffer: Vec<u8>,
    /// Cached OSC title; empty means the application supplies its default.
    title: String,
    size: TerminalSize,
    theme: Theme,
    pending_exit: Option<Option<std::process::ExitStatus>>,
    exit_delivered: bool,
    pending_error: Option<String>,
    output_finished: bool,
    shell_integration: ShellIntegration,
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
            previous_byte_is_escape: false,
            question_mark_since_terminator: false,
            reply_buffer: Vec::new(),
            title: String::new(),
            size,
            theme: config.theme.clone(),
            pending_exit: None,
            exit_delivered: false,
            pending_error: None,
            output_finished: false,
            shell_integration: ShellIntegration::default(),
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
        self.pty.begin_input_turn();
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
        self.flush_replies();
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
            self.parse_chunk(&chunk);
            outcome.grid_maybe_changed = true;
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

    fn parse_chunk(&mut self, chunk: &[u8]) {
        // Every OSC color query contains '?'. With no candidate carried from
        // prior input, chunks without '?' need no intermediate palette read.
        // This keeps dense title updates on the normal single-advance path.
        if !self.question_mark_since_terminator && memchr::memchr(b'?', chunk).is_none() {
            self.parser.advance(&mut self.term, chunk);
            self.drain_parser_events();
            if let Some(byte) = chunk.last() {
                self.previous_byte_is_escape = *byte == b'\x1b';
            }
            self.flush_replies();
            return;
        }
        // Resolve OSC queries before a later OSC set/reset in the same chunk
        // changes the palette. VTE preserves incomplete escapes across slices.
        // Drain only at BEL or actual ESC-backslash pairs. Literal backslashes
        // in paths and JSON stay in the same parser slice as ordinary text.
        let mut start = 0;
        if self.previous_byte_is_escape && chunk.first() == Some(&b'\\') {
            self.parser.advance(&mut self.term, &chunk[..1]);
            if self.event_proxy.has_pending_query() {
                self.drain_parser_events();
            }
            start = 1;
        }
        let mut bells = memchr::memchr_iter(b'\x07', chunk).peekable();
        let mut sts = memchr::memmem::find_iter(chunk, b"\x1b\\").peekable();
        loop {
            let end = match (bells.peek(), sts.peek()) {
                (Some(&bell), Some(&st)) if bell < st => bells.next().unwrap() + 1,
                (_, Some(_)) => sts.next().unwrap() + 2,
                (Some(_), None) => bells.next().unwrap() + 1,
                (None, None) => break,
            };
            self.parser.advance(&mut self.term, &chunk[start..end]);
            if self.event_proxy.has_pending_query() {
                self.drain_parser_events();
            }
            start = end;
        }
        if start < chunk.len() {
            self.parser.advance(&mut self.term, &chunk[start..]);
        }
        self.question_mark_since_terminator = (start == 0 && self.question_mark_since_terminator)
            || memchr::memchr(b'?', &chunk[start..]).is_some();
        self.drain_parser_events();
        if let Some(byte) = chunk.last() {
            self.previous_byte_is_escape = *byte == b'\x1b';
        }
        self.flush_replies();
    }

    fn drain_parser_events(&mut self) {
        for event in self.event_proxy.drain() {
            match event {
                TerminalEvent::ShellIntegration(AlacrittyEvent::ShellIntegration(
                    params,
                    point,
                    scroll,
                    generation,
                )) => {
                    self.shell_integration.marker(&params, point, scroll, generation);
                }
                TerminalEvent::ShellIntegration(AlacrittyEvent::ShellErase(
                    point,
                    mode,
                    scroll,
                    generation,
                )) => {
                    self.shell_integration.erase(point, mode, scroll, generation);
                }
                TerminalEvent::ShellIntegration(AlacrittyEvent::ShellCellsChanged(
                    start,
                    end,
                    scroll,
                    generation,
                )) => {
                    self.shell_integration.cells_changed(start, end, scroll, generation);
                }
                TerminalEvent::TitleChanged(t) => self.title = t,
                TerminalEvent::TitleReset => self.title.clear(),
                TerminalEvent::Exit(status) if !self.exit_delivered => {
                    self.pending_exit.get_or_insert(status);
                }
                TerminalEvent::PtyWrite(bytes) => {
                    self.write_reply(&bytes);
                }
                TerminalEvent::Query(AlacrittyEvent::ColorRequest(index, formatter)) => {
                    if let Some(color) = self.query_color(index) {
                        self.write_reply(formatter(color).as_bytes());
                    }
                }
                TerminalEvent::Query(AlacrittyEvent::TextAreaSizeRequest(formatter)) => {
                    // The library formatter multiplies u16 dimensions. Clamp
                    // cell sizes so unusually large viewports cannot overflow.
                    let mut size = self.size.to_window_size();
                    size.cell_width = size.cell_width.min(u16::MAX / size.num_cols.max(1));
                    size.cell_height = size.cell_height.min(u16::MAX / size.num_lines.max(1));
                    self.write_reply(formatter(size).as_bytes());
                }
                _ => {}
            }
        }
        let (scroll, generation) = self.term.shell_coordinates();
        let oldest = scroll.min(i64::MAX as u64) as i64 + self.term.grid().topmost_line().0 as i64;
        // The alternate grid has no history; its bounds cannot evict main-grid markers.
        if !self.term.mode().contains(alacritty_terminal::term::TermMode::ALT_SCREEN) {
            self.shell_integration.synchronize(generation, oldest);
        }
    }

    // Protocol replies must not move a scrolled viewport.
    fn write_reply(&mut self, bytes: &[u8]) {
        self.reply_buffer.extend_from_slice(bytes);
    }

    fn flush_replies(&mut self) {
        if self.reply_buffer.is_empty() {
            return;
        }
        // The transport accepts or rejects the whole batch. Saturation can
        // reject replies, just as it rejects user input; never retry a prefix.
        if let Err(error) = self.pty.write(&self.reply_buffer) {
            log::warn!("could not write terminal reply: {error}");
        }
        self.reply_buffer.clear();
    }

    /// Update the default palette used for terminal color query replies.
    /// OSC color overrides remain active until the child resets them.
    pub fn set_theme(&mut self, theme: &Theme) {
        self.theme = theme.clone();
    }

    fn query_color(&self, index: usize) -> Option<Rgb> {
        if index >= alacritty_terminal::term::color::COUNT {
            return None;
        }
        self.term.colors()[index].or_else(|| theme_color(&self.theme, index))
    }

    /// Feed `data` directly to the VTE parser without writing to the PTY.
    pub fn inject_local(&mut self, data: &[u8]) {
        self.parser.advance(&mut self.term, data);
        self.question_mark_since_terminator |= memchr::memchr(b'?', data).is_some();
        if let Some(byte) = data.last() {
            self.previous_byte_is_escape = *byte == b'\x1b';
        }
        self.drain_parser_events();
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
        // Alacritty may reflow rows during resize; old cell coordinates no longer apply.
        self.shell_integration.invalidate();

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

    /// Shell cwd, command boundaries and last completion status.
    pub fn shell_integration(&self) -> &ShellIntegration {
        &self.shell_integration
    }

    fn shell_point(&self, position: ShellPosition) -> Option<Point> {
        let (scroll, _) = self.term.shell_coordinates();
        let line = position.line.checked_sub(scroll.min(i64::MAX as u64) as i64)?;
        let line = i32::try_from(line).ok()?;
        if line < self.grid().topmost_line().0
            || line > self.grid().bottommost_line().0
            || position.column > self.columns()
        {
            return None;
        }
        Some(Point::new(
            alacritty_terminal::index::Line(line),
            alacritty_terminal::index::Column(position.column),
        ))
    }

    /// Navigate to the closest prompt above the current viewport.
    pub fn jump_to_previous_prompt(&mut self) -> bool {
        self.jump_to_prompt(true)
    }

    /// Navigate to the closest prompt below the current viewport.
    pub fn jump_to_next_prompt(&mut self) -> bool {
        self.jump_to_prompt(false)
    }

    fn jump_to_prompt(&mut self, previous: bool) -> bool {
        if self.term.mode().contains(alacritty_terminal::term::TermMode::ALT_SCREEN) {
            return false;
        }
        let top = -(self.grid().display_offset() as i32);
        let prompts = self
            .shell_integration
            .commands()
            .iter()
            .filter_map(|command| command.prompt.and_then(|p| self.shell_point(p)));
        let target = if previous {
            prompts.filter(|p| p.line.0 < top).max()
        } else {
            prompts.filter(|p| p.line.0 > top).min()
        };
        let Some(target) = target else {
            return false;
        };
        let offset = (-target.line.0).max(0);
        if offset == self.grid().display_offset() as i32 {
            return false;
        }
        self.term.scroll_display(Scroll::Delta(offset - self.grid().display_offset() as i32));
        true
    }

    /// Output of the most recent completed command, when its full range remains
    /// in the main grid. Reflow, clearing and history eviction can make it unavailable.
    pub fn last_command_output(&self) -> Option<String> {
        if self.term.mode().contains(alacritty_terminal::term::TermMode::ALT_SCREEN) {
            return None;
        }
        let command = self.shell_integration.commands().iter().rev().find(|c| c.completed)?;
        let mut start = self.shell_point(command.output_start?)?;
        if start.column.0 == self.columns() {
            start = Point::new(start.line + 1, alacritty_terminal::index::Column(0));
        }
        let end = self.shell_point(command.output_end?)?;
        if end < start {
            return None;
        }
        if start.line > self.grid().bottommost_line() {
            return None;
        }
        if end == start {
            return Some(String::new());
        }
        let end = if end.column.0 == 0 {
            Point::new(end.line - 1, self.grid().last_column())
        } else {
            Point::new(end.line, end.column - 1)
        };
        Some(self.term.bounds_to_string(start, end))
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
        self.shell_integration.invalidate();
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

/// Default colors matching the configured ANSI palette and xterm 256-color cube.
fn theme_color(theme: &Theme, index: usize) -> Option<Rgb> {
    let ansi = &theme.ansi;
    let named = [
        ansi.black,
        ansi.red,
        ansi.green,
        ansi.yellow,
        ansi.blue,
        ansi.magenta,
        ansi.cyan,
        ansi.white,
        ansi.bright_black,
        ansi.bright_red,
        ansi.bright_green,
        ansi.bright_yellow,
        ansi.bright_blue,
        ansi.bright_magenta,
        ansi.bright_cyan,
        ansi.bright_white,
    ];
    let color = match index {
        0..=15 => named[index],
        16..=231 => {
            let index = index - 16;
            let component = |value| if value == 0 { 0 } else { 55 + value as u8 * 40 };
            return Some(Rgb {
                r: component(index / 36),
                g: component(index / 6 % 6),
                b: component(index % 6),
            });
        }
        232..=255 => {
            let value = 8 + (index - 232) as u8 * 10;
            return Some(Rgb { r: value, g: value, b: value });
        }
        256 | 267 | 268 => theme.foreground,
        257 => theme.background,
        258 => theme.cursor,
        259..=266 => named[index - 259],
        _ => return None,
    };
    Some(Rgb { r: color.r, g: color.g, b: color.b })
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
        buffered_terminal_with_waker(noop_waker())
    }

    fn buffered_terminal_with_waker(waker: crate::PtyWaker) -> (Terminal, crate::pty::TestPtyPeer) {
        let (pty, peer) = PtyHandle::test_pair(waker);
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
                previous_byte_is_escape: false,
                question_mark_since_terminator: false,
                reply_buffer: Vec::new(),
                title: String::new(),
                size,
                theme: Theme::default(),
                pending_exit: None,
                exit_delivered: false,
                pending_error: None,
                output_finished: false,
                shell_integration: ShellIntegration::default(),
            },
            peer,
        )
    }

    fn parse_one_chunk(terminal: &mut Terminal) -> ProcessOutcome {
        terminal.process_input_with_budget(Instant::now(), Duration::ZERO, PARSE_BYTE_BUDGET)
    }

    #[test]
    fn shell_protocol_fragmented_lifecycle_and_mixed_output() {
        let payload = b"\x1b]7;file://host/tmp/a%20b%3Bc\x1b\\\x1b]133;A\x07$ \x1b]133;B\x07printf hello\r\n\x1b]133;C\x1b\\hello\r\nworld\r\n\x1b]133;D;7\x07\x1b]133;A\x07$ \x1b]133;B\x07";
        // Every possible two-chunk split, plus one-byte fragments, must have
        // exactly the same lifecycle and output coordinates.
        for split in 0..=payload.len() {
            let (mut terminal, _) = buffered_terminal();
            terminal.parse_chunk(&payload[..split]);
            terminal.parse_chunk(&payload[split..]);
            assert_eq!(terminal.shell_integration().cwd(), Some("/tmp/a b;c"));
            assert_eq!(terminal.shell_integration().cwd_host(), Some("host"));
            assert_eq!(terminal.shell_integration().last_exit_status(), Some(7));
            assert_eq!(
                terminal.last_command_output().as_deref(),
                Some("hello\nworld"),
                "split {split}"
            );
            assert_eq!(terminal.shell_integration().commands().len(), 2);
        }
        let (mut terminal, _) = buffered_terminal();
        for byte in payload {
            terminal.parse_chunk(&[*byte]);
        }
        assert_eq!(terminal.last_command_output().as_deref(), Some("hello\nworld"));
    }

    #[test]
    fn shell_markers_survive_full_scrollback_and_history_capacity() {
        let (mut terminal, _) = buffered_terminal();
        terminal.term.grid_mut().update_history(8);
        terminal.parse_chunk(&b"padding\r\n".repeat(30));
        terminal.parse_chunk(
            b"\x1b]133;A\x07$ \x1b]133;B\x07cmd\r\n\x1b]133;C\x07result\r\n\x1b]133;D;0\x07",
        );
        terminal.parse_chunk(&b"later\r\n".repeat(24));
        assert_eq!(terminal.last_command_output().as_deref(), Some("result"));
        assert!(terminal.jump_to_previous_prompt());
        assert!(terminal.grid().display_offset() > 0);
        terminal.parse_chunk(&b"later\r\n".repeat(30));
        assert!(terminal.last_command_output().is_none());
        assert!(!terminal.jump_to_previous_prompt());
        assert_eq!(terminal.shell_integration().last_exit_status(), Some(0));
    }

    #[test]
    fn shell_markers_follow_soft_wrap_and_ignore_alternate_screen() {
        let (mut terminal, _) = buffered_terminal();
        terminal.parse_chunk(&b"padding\r\n".repeat(25));
        terminal.parse_chunk(b"\x1b]133;A\x07\x1b]133;B\x07cmd\r\n\x1b]133;C\x07");
        let output = "x".repeat(200);
        terminal.parse_chunk(output.as_bytes());
        terminal.parse_chunk(b"\r\n\x1b]133;D;0\x07");
        assert_eq!(terminal.last_command_output(), Some(output.clone()));
        terminal.parse_chunk(b"\x1b[?1049h\x1b]133;A\x07\x1b]7;file:///wrong\x07");
        terminal.parse_chunk(&b"alt\r\n".repeat(100));
        assert!(terminal.last_command_output().is_none());
        assert!(!terminal.jump_to_previous_prompt());
        terminal.parse_chunk(b"\x1b[?1049l");
        assert!(terminal.shell_integration().cwd().is_none());
        assert_eq!(terminal.last_command_output(), Some(output));
    }

    #[test]
    fn shell_destructive_changes_invalidate_positions_and_preserve_metadata() {
        for clear in [b"\x1bc".as_slice(), b"\x1b[2J", b"\x1b[3J", b"\x1b[2;5r\x1b[5;1H\n"] {
            let (mut terminal, _) = buffered_terminal();
            terminal.parse_chunk(
                b"\x1b]7;file:///tmp\x07\x1b]133;A\x07\x1b]133;C\x07output\r\n\x1b]133;D;3\x07",
            );
            assert!(terminal.last_command_output().is_some());
            terminal.parse_chunk(clear);
            assert!(terminal.last_command_output().is_none(), "{clear:?}");
            assert!(terminal.shell_integration().commands().is_empty());
            assert_eq!(terminal.shell_integration().cwd(), Some("/tmp"));
            assert_eq!(terminal.shell_integration().last_exit_status(), Some(3));
        }
        let (mut terminal, _) = buffered_terminal();
        terminal.parse_chunk(b"\x1b]133;A\x07\x1b]133;C\x07output\r\n\x1b]133;D;0\x07");
        terminal.shell_integration.invalidate(); // Resize uses this same invalidation before the PTY ioctl.
        assert!(terminal.last_command_output().is_none());
    }

    #[test]
    fn shell_rejects_invalid_status_uri_and_cancelled_markers() {
        let (mut terminal, _) = buffered_terminal();
        terminal.parse_chunk(
            b"\x1b]7;file:///good\x07\x1b]7;file:///bad%xx\x07\x1b]133;A\x18ignored\x07",
        );
        assert_eq!(terminal.shell_integration().cwd(), Some("/good"));
        assert!(terminal.shell_integration().commands().is_empty());
        terminal.parse_chunk(
            b"\x1b]133;A\x07\x1b]133;C\x07result\r\n\x1b]133;D;-1\x07\x1b]133;D;2147483648\x07",
        );
        assert!(terminal.last_command_output().is_none());
        terminal.parse_chunk(b"\x1b]133;D;12");
        assert!(terminal.last_command_output().is_none());
        terminal.parse_chunk(b"\x1b\\");
        assert_eq!(terminal.shell_integration().last_exit_status(), Some(12));
        assert_eq!(terminal.last_command_output().as_deref(), Some("result"));
        terminal.parse_chunk(b"\x1b]133;D;0\x07");
        assert_eq!(terminal.shell_integration().last_exit_status(), Some(12));
    }

    #[test]
    fn shell_osc_inside_ignored_control_strings_is_not_a_marker() {
        let (mut terminal, _) = buffered_terminal();
        terminal.parse_chunk(b"\x1bPignored]133;A\x07\x1b\\");
        assert!(terminal.shell_integration().commands().is_empty());
        let oversized = format!("\x1b]7;file:///{}\x07", "x".repeat(9000));
        terminal.parse_chunk(oversized.as_bytes());
        assert!(terminal.shell_integration().cwd().is_none());
        terminal.parse_chunk(b"\x1b]7;file:///okay\x07");
        assert_eq!(terminal.shell_integration().cwd(), Some("/okay"));
    }

    #[test]
    fn shell_requires_complete_st_and_recovers_from_parameter_overflow() {
        let (mut terminal, _) = buffered_terminal();
        terminal.parse_chunk(b"\x1b]7;file:///cancelled\x1b");
        assert!(terminal.shell_integration().cwd().is_none());
        terminal.parse_chunk(b"[0m\x1b]133;A\x1b[0m");
        assert!(terminal.shell_integration().cwd().is_none());
        assert!(terminal.shell_integration().commands().is_empty());
        let uri = format!("\x1b]7;file:///{}\x07", "a;".repeat(20));
        terminal.parse_chunk(uri.as_bytes());
        assert!(terminal.shell_integration().cwd().is_none());
        terminal.parse_chunk(b"\x1b]7;file:///valid\x1b");
        assert!(terminal.shell_integration().cwd().is_none());
        terminal.parse_chunk(b"\\");
        assert_eq!(terminal.shell_integration().cwd(), Some("/valid"));
    }

    #[test]
    fn shell_prompt_redraw_keeps_current_command_and_erased_output_is_unavailable() {
        let (mut terminal, _) = buffered_terminal();
        terminal.parse_chunk(
            b"\x1b]133;A\x07$ \x1b]133;B\x07\x1b[Jcmd\r\n\x1b]133;C\x07output\r\n\x1b]133;D;0\x07",
        );
        assert_eq!(terminal.last_command_output().as_deref(), Some("output"));
        terminal.parse_chunk(b"\x1b[2;1H\x1b[2K");
        assert!(terminal.last_command_output().is_none());
        assert_eq!(terminal.shell_integration().last_exit_status(), Some(0));
        terminal.parse_chunk(b"\x1b]133;A\x07\x1b]133;C\x07\x1b[2J\x1b]133;D;7\x07");
        assert_eq!(terminal.shell_integration().last_exit_status(), Some(7));
        assert!(!terminal.shell_integration().is_running());
        terminal.parse_chunk(b"\x1b]133;A\x07\x1b]133;C\x07");
        assert!(terminal.shell_integration().is_running());
        terminal.shell_integration.invalidate();
        assert!(terminal.shell_integration().is_running());
        terminal.parse_chunk(b"\x1b]133;D;0\x07");
        assert!(!terminal.shell_integration().is_running());
    }

    #[test]
    fn shell_output_exclusive_endpoint_includes_pending_wrap_cell() {
        for output in ["x".repeat(80), format!("{}界", "x".repeat(78))] {
            let (mut terminal, _) = buffered_terminal();
            terminal.parse_chunk(&b"padding\r\n".repeat(30));
            terminal.parse_chunk(b"\x1b]133;A\x07\x1b]133;C\x07");
            terminal.parse_chunk(output.as_bytes());
            terminal.parse_chunk(b"\x1b]133;D;0\x07");
            assert_eq!(terminal.last_command_output(), Some(output));
        }
    }

    #[test]
    #[ignore = "manual shell integration throughput benchmark"]
    fn shell_protocol_benchmark() {
        const COMMANDS: usize = 2000;
        const ROUNDS: usize = 10;
        let plain = b"$ true\r\noutput\r\n".repeat(COMMANDS);
        let hooked =
            b"\x1b]133;A\x07$ \x1b]133;B\x07true\r\n\x1b]133;C\x07output\r\n\x1b]133;D;0\x07"
                .repeat(COMMANDS);
        let measure = |payload: &[u8]| {
            let mut timings = Vec::with_capacity(ROUNDS);
            for _ in 0..ROUNDS {
                let (mut terminal, _) = buffered_terminal();
                let started = Instant::now();
                terminal.parse_chunk(payload);
                timings.push(started.elapsed());
                if payload.len() == hooked.len() {
                    assert_eq!(terminal.shell_integration().commands().len(), 1024);
                    assert_eq!(terminal.shell_integration().last_exit_status(), Some(0));
                    assert_eq!(terminal.last_command_output().as_deref(), Some("output"));
                }
            }
            timings.sort();
            timings[ROUNDS / 2]
        };
        let baseline = measure(&plain);
        let integration = measure(&hooked);
        eprintln!(
            "shell lifecycle: {COMMANDS} commands, plain median {baseline:?}, hooks median {integration:?}, added {:.1} ns/command",
            integration.as_nanos().saturating_sub(baseline.as_nanos()) as f64 / COMMANDS as f64
        );
    }

    #[test]
    fn output_bursts_share_one_wake_until_the_parser_turn_starts() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&wakes);
        let (mut terminal, peer) = buffered_terminal_with_waker(Arc::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }));
        for _ in 0..1024 {
            peer.send(b"x".to_vec());
        }
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert!(terminal.process_input().grid_maybe_changed);
        peer.send(b"\x1b]2;next-burst\x07".to_vec());
        assert_eq!(wakes.load(Ordering::Relaxed), 2);
        terminal.process_input();
        assert_eq!(terminal.title(), "next-burst");
        peer.exit(None);
        assert_eq!(wakes.load(Ordering::Relaxed), 3, "exit and completion share a wake");
        assert_eq!(terminal.process_input().child_exit, Some(None));
    }

    #[test]
    fn budget_continuations_drain_without_new_producer_wakes() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&wakes);
        let (mut terminal, peer) = buffered_terminal_with_waker(Arc::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }));
        peer.send(vec![b' '; 192 * 1024]);
        peer.send(b"\x1b]2;drained\x07".to_vec());
        peer.exit(None);
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        for _ in 0..4 {
            let result = parse_one_chunk(&mut terminal);
            assert!(result.more_output);
            assert!(result.child_exit.is_none());
        }
        assert_eq!(terminal.title(), "drained");
        assert_eq!(terminal.process_input().child_exit, Some(None));
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn worker_completion_rearms_after_an_early_failure_wake() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let wakes = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&wakes);
        let (mut terminal, peer) = buffered_terminal_with_waker(Arc::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }));
        peer.fail();
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert!(terminal.process_input().io_error.is_none());
        peer.send(b"\x1b]2;last-output\x07".to_vec());
        peer.finish();
        assert_eq!(wakes.load(Ordering::Relaxed), 2);
        let result = terminal.process_input();
        assert_eq!(terminal.title(), "last-output");
        assert_eq!(result.io_error.as_deref(), Some("test transport failure"));
    }

    #[test]
    fn concurrent_publication_and_parser_acknowledgement_do_not_lose_wakes() {
        use std::{sync::Arc, thread};
        let (tx, rx) = crossbeam_channel::unbounded();
        let (mut terminal, peer) = buffered_terminal_with_waker(Arc::new(move || {
            tx.send(()).unwrap();
        }));
        let producer = thread::spawn(move || {
            for index in 0..2000 {
                peer.send(format!("\x1b]2;sequence-{index}\x07").into_bytes());
                if index % 7 == 0 {
                    thread::yield_now();
                }
            }
            peer.exit(None);
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        'events: loop {
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("lost PTY wake");
            loop {
                let result = parse_one_chunk(&mut terminal);
                if result.child_exit.is_some() {
                    break 'events;
                }
                if !result.more_output {
                    break;
                }
                assert!(Instant::now() < deadline, "parser continuation stalled");
            }
        }
        producer.join().unwrap();
        assert_eq!(terminal.title(), "sequence-1999");
    }

    #[test]
    fn color_queries_reply_with_theme_palette_and_preserve_terminators() {
        let (mut terminal, peer) = buffered_terminal();
        let theme = Theme {
            foreground: mechanic_config::Rgb::new(1, 2, 3),
            background: mechanic_config::Rgb::new(4, 5, 6),
            cursor: mechanic_config::Rgb::new(7, 8, 9),
            ..Theme::default()
        };
        terminal.set_theme(&theme);
        peer.send(
            b"\x1b]10;?\x07\x1b]11;?\x1b\\\x1b]12;?\x07\x1b]4;1;?;16;?;231;?;232;?;255;?\x07"
                .to_vec(),
        );
        terminal.process_input();
        let expected = [
            "\x1b]10;rgb:0101/0202/0303\x07",
            "\x1b]11;rgb:0404/0505/0606\x1b\\",
            "\x1b]12;rgb:0707/0808/0909\x07",
            "\x1b]4;1;rgb:cccc/2222/0000\x07",
            "\x1b]4;16;rgb:0000/0000/0000\x07",
            "\x1b]4;231;rgb:ffff/ffff/ffff\x07",
            "\x1b]4;232;rgb:0808/0808/0808\x07",
            "\x1b]4;255;rgb:eeee/eeee/eeee\x07",
        ]
        .concat();
        assert_eq!(peer.reply(), expected.as_bytes());
    }

    #[test]
    fn color_queries_observe_osc_overrides_and_resets_in_arrival_order() {
        let (mut terminal, peer) = buffered_terminal();
        peer.send(b"\x1b]10;?\x07\x1b]10;#123456\x07\x1b]10;?\x07\x1b]110\x07\x1b]10;?\x07\x1b]4;1;#abcdef\x1b\\\x1b]4;1;?\x1b\\\x1b]104;1\x1b\\\x1b]4;1;?\x1b\\".to_vec());
        terminal.process_input();
        let expected = [
            "\x1b]10;rgb:5252/e8e8/ffff\x07",
            "\x1b]10;rgb:1212/3434/5656\x07",
            "\x1b]10;rgb:5252/e8e8/ffff\x07",
            "\x1b]4;1;rgb:abab/cdcd/efef\x1b\\",
            "\x1b]4;1;rgb:cccc/2222/0000\x1b\\",
        ]
        .concat();
        assert_eq!(peer.reply(), expected.as_bytes());
    }

    #[test]
    fn color_query_preserves_parser_state_across_pty_chunks() {
        let (mut terminal, peer) = buffered_terminal();
        peer.send(b"\x1b]4;1;#1234".to_vec());
        terminal.process_input();
        peer.send(b"56\x07\x1b]4;1;?\x1b".to_vec());
        terminal.process_input();
        // ST crosses the chunk boundary; the following reset must not change
        // the outstanding query's reply.
        peer.send(b"\\\x1b]104;1\x07\x1b]4;1;?\x07".to_vec());
        terminal.process_input();
        assert_eq!(peer.reply(), b"\x1b]4;1;rgb:1212/3434/5656\x1b\\");
        assert_eq!(peer.reply(), b"\x1b]4;1;rgb:cccc/2222/0000\x07");
    }

    #[test]
    fn dense_queries_batch_replies_without_exhausting_pty_message_queue() {
        let (mut terminal, peer) = buffered_terminal();
        // More queries than the transport's 1024-message capacity still occupy
        // one queue entry, with every reply intact and in arrival order.
        peer.send(b"\x1b]10;?\x07\x1b[6n".repeat(1100));
        terminal.process_input();
        assert_eq!(peer.reply(), b"\x1b]10;rgb:5252/e8e8/ffff\x07\x1b[1;1R".repeat(1100));
        assert!(terminal.reply_buffer.is_empty());
    }

    #[test]
    fn query_candidate_survives_chunks_without_new_question_marks() {
        let (mut terminal, peer) = buffered_terminal();
        peer.send(b"\x1b]10;?".to_vec());
        terminal.process_input();
        assert!(terminal.question_mark_since_terminator);
        peer.send(b"\x07\x1b]10;#123456\x07".to_vec());
        terminal.process_input();
        assert_eq!(peer.reply(), b"\x1b]10;rgb:5252/e8e8/ffff\x07");
        assert!(!terminal.question_mark_since_terminator);
        peer.send(b"\x1b]10;?\x07".to_vec());
        terminal.process_input();
        assert_eq!(peer.reply(), b"\x1b]10;rgb:1212/3434/5656\x07");
        terminal.inject_local(b"\x1b]10;?");
        peer.send(b"\x07\x1b]110\x07".to_vec());
        terminal.process_input();
        assert_eq!(peer.reply(), b"\x1b]10;rgb:1212/3434/5656\x07");
        assert!(!terminal.question_mark_since_terminator);
    }

    #[test]
    fn osc8_links_preserve_targets_across_chunks_and_close_after_label() {
        use alacritty_terminal::index::{Column, Line};

        for terminator in ["\x07", "\x1b\\"] {
            let (mut terminal, peer) = buffered_terminal();
            let target = "https://example.com/report?language=ar&year=2026";
            let payload =
                format!("\x1b]8;id=article;{target}{terminator}Read\x1b]8;;{terminator} plain");
            for byte in payload.bytes() {
                peer.send(vec![byte]);
                terminal.process_input();
            }
            for col in 0..4 {
                let link = terminal.grid()[Line(0)][Column(col)].hyperlink().unwrap();
                assert_eq!(link.uri(), target);
                assert_eq!(link.id(), "article");
            }
            for col in 4..10 {
                assert!(terminal.grid()[Line(0)][Column(col)].hyperlink().is_none());
            }
        }
    }

    #[test]
    fn size_queries_use_resized_terminal_cell_dimensions() {
        let (mut terminal, peer) = buffered_terminal();
        peer.send(b"\x1b[14t\x1b[18t".to_vec());
        terminal.process_input();
        assert_eq!(peer.reply(), b"\x1b[4;384;640t\x1b[8;24;80t");
        // The buffered peer has no PTY fd for the resize ioctl; update its
        // viewport directly. The real resize path is exercised below.
        terminal.size = TerminalSize { columns: 100, rows: 30, cell_width: 11, cell_height: 19 };
        terminal.term.resize(TermDimensions { columns: 100, screen_lines: 30 });
        peer.send(b"\x1b[14t\x1b[18t".to_vec());
        terminal.process_input();
        assert_eq!(peer.reply(), b"\x1b[4;570;1100t\x1b[8;30;100t");
    }

    #[test]
    fn query_reply_keeps_scrollback_viewport_position() {
        let (mut terminal, peer) = buffered_terminal();
        terminal.inject_local(&b"line\r\n".repeat(50));
        terminal.term.scroll_display(Scroll::Delta(5));
        let offset = terminal.term.grid().display_offset();
        assert!(offset > 0);
        peer.send(b"\x1b]10;?\x07\x1b[14t".to_vec());
        terminal.process_input();
        assert_eq!(peer.reply(), b"\x1b]10;rgb:5252/e8e8/ffff\x07\x1b[4;384;640t");
        assert_eq!(terminal.term.grid().display_offset(), offset);
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
    fn native_pty_delivers_output_and_exit_using_only_wakes() {
        use std::{io::Write, os::unix::fs::PermissionsExt, sync::Arc};

        let mut script = tempfile::NamedTempFile::new().unwrap();
        script.write_all(b"#!/bin/sh\nstty raw -echo\nprintf '\\033]2;ready\\007'\ndd bs=1 count=1 >/dev/null 2>&1\nprintf '\\033]2;final\\007'\nexit 7\n").unwrap();
        script.as_file().set_permissions(std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = script.into_temp_path();
        let mut config = Config::default();
        config.shell.program = path.to_str().unwrap().into();
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut terminal = Terminal::new(
            &config,
            TerminalSize::default(),
            Arc::new(move || {
                let _ = tx.send(());
            }),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut released = false;
        loop {
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("native PTY wake stalled");
            loop {
                let result = terminal.process_input();
                assert!(result.io_error.is_none(), "{:?}", result.io_error);
                if !released && terminal.title() == "ready" {
                    terminal.write_to_pty(b"x").unwrap();
                    released = true;
                }
                if let Some(status) = result.child_exit {
                    assert!(released);
                    assert_eq!(terminal.title(), "final");
                    assert_eq!(status.unwrap().code(), Some(7));
                    return;
                }
                if !result.more_output {
                    break;
                }
                assert!(Instant::now() < deadline, "native parser continuation stalled");
            }
        }
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
