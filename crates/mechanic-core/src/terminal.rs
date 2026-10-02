//! Terminal state wrapper.

use std::path::Path;
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
use alacritty_terminal::vte::ansi::{CursorShape, KeyboardModes, Processor, Rgb};
use mechanic_config::{Config, Theme};

use crate::PtyWaker;
use crate::TerminalSize;
use crate::clipboard::{ClipboardRequest, ClipboardRequests};
use crate::error::TerminalError;
use crate::event::{EventProxy, TerminalEvent};
use crate::pty::PtyHandle;
use crate::shell_state::{CommandCompletion, ShellIntegration, ShellPosition};

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
    clipboard_requests: ClipboardRequests,
}

impl Terminal {
    /// Create a new terminal with a PTY of the given size.
    pub fn new(
        config: &Config,
        size: TerminalSize,
        waker: PtyWaker,
    ) -> Result<Self, TerminalError> {
        Self::new_in_directory(config, size, waker, None)
    }

    /// Create a terminal whose child starts in an optional inherited directory.
    /// Missing or unavailable directories fall back to the normal shell cwd.
    pub fn new_in_directory(
        config: &Config,
        size: TerminalSize,
        waker: PtyWaker,
        directory: Option<&Path>,
    ) -> Result<Self, TerminalError> {
        if size.columns == 0 || size.rows == 0 {
            return Err(TerminalError::InvalidSize { columns: size.columns, rows: size.rows });
        }

        let event_proxy = EventProxy::new();

        let term_config = TermConfig {
            scrolling_history: config.terminal.scrollback_lines,
            kitty_keyboard: true,
            // Application policy handles permission and target support. The
            // parser must still surface reads so denials receive a response.
            osc52: alacritty_terminal::term::Osc52::CopyPaste,
            ..TermConfig::default()
        };

        let dimensions = TermDimensions { columns: size.columns, screen_lines: size.rows };
        let term = Term::new(term_config, &dimensions, event_proxy.clone());

        let pty = PtyHandle::spawn_in_directory(config, size, waker, directory)?;

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
            clipboard_requests: ClipboardRequests::default(),
        })
    }

    /// Drain output, update the grid/title, and send terminal protocol replies.
    pub fn process_input(&mut self) -> ProcessOutcome {
        self.process_input_with_budget(Instant::now(), PARSE_TIME_BUDGET, PARSE_BYTE_BUDGET)
    }

    /// Expiration of the current synchronized update, if one is active.
    /// The application schedules this deadline even when the pane is hidden.
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    /// Replay an expired synchronized update without requiring another PTY wake.
    /// Returns whether buffered terminal output was released.
    pub fn expire_synchronized_update(&mut self, now: Instant) -> bool {
        if self.sync_deadline().is_none_or(|deadline| deadline > now) {
            return false;
        }
        self.stop_synchronized_update();
        true
    }

    fn stop_synchronized_update(&mut self) {
        self.with_parser(false, |parser, term, checkpoint| {
            parser.stop_sync_with_callback(term, checkpoint);
        });
        self.drain_parser_events();
        self.flush_replies();
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
        if self.sync_deadline().is_some() {
            outcome.grid_maybe_changed = self.expire_synchronized_update(Instant::now());
        }
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
                // An unterminated synchronized update must be visible before
                // the application observes final output, exit, or transport failure.
                if self.sync_deadline().is_some() {
                    self.stop_synchronized_update();
                    outcome.grid_maybe_changed = true;
                }
                if let Some(error) = self.pty.take_failure() {
                    self.pending_error = Some(error);
                }
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
            self.advance_parser(chunk);
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
            self.advance_parser(&chunk[..1]);
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
            self.advance_parser(&chunk[start..end]);
            if self.event_proxy.has_pending_query() {
                self.drain_parser_events();
            }
            start = end;
        }
        if start < chunk.len() {
            self.advance_parser(&chunk[start..]);
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
        self.with_parser(true, |_, term, checkpoint| checkpoint(term));
    }

    fn advance_parser(&mut self, bytes: &[u8]) {
        self.with_parser(false, |parser, term, checkpoint| {
            parser.advance_with_sync_callback(term, bytes, checkpoint);
        });
    }

    // Keep parser/grid borrows separate from the event state so callbacks can
    // resolve palette queries at OSC boundaries during synchronized replay.
    fn with_parser(
        &mut self,
        drain_all_events: bool,
        operation: impl FnOnce(
            &mut Processor,
            &mut Term<EventProxy>,
            &mut dyn FnMut(&mut Term<EventProxy>),
        ),
    ) {
        let mut events = ParserEvents {
            proxy: &self.event_proxy,
            shell: &mut self.shell_integration,
            title: &mut self.title,
            pending_exit: &mut self.pending_exit,
            exit_delivered: self.exit_delivered,
            replies: &mut self.reply_buffer,
            size: self.size,
            theme: &self.theme,
            clipboard: &mut self.clipboard_requests,
        };
        operation(&mut self.parser, &mut self.term, &mut |term| {
            // ESU itself contains '?', so replay may visit many OSC boundaries
            // without producing a query. Keep title/shell event draining on
            // the final chunk path unless a protocol query needs resolution.
            if drain_all_events || events.proxy.has_pending_query() {
                events.drain(term);
            }
        });
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

    /// Feed `data` directly to the VTE parser without writing to the PTY.
    pub fn inject_local(&mut self, data: &[u8]) {
        self.advance_parser(data);
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

    /// Clipboard operations emitted by OSC 52, in parser source order.
    pub fn drain_clipboard_requests(&mut self) -> Vec<ClipboardRequest> {
        self.clipboard_requests.drain()
    }

    /// Send a clipboard protocol response without moving the scrollback viewport.
    pub fn write_clipboard_reply(&mut self, bytes: &[u8]) -> Result<(), TerminalError> {
        self.write_protocol_reply(bytes)
    }

    /// Send terminal protocol bytes without moving the scrollback viewport.
    pub fn write_protocol_reply(&mut self, bytes: &[u8]) -> Result<(), TerminalError> {
        self.pty.write(bytes)
    }

    /// Filter clipboard text and wrap it when DECSET 2004 is enabled.
    /// See [`crate::paste::filter`] for the filtering contract.
    pub fn paste(&mut self, text: &str) -> Result<(), TerminalError> {
        self.paste_with_enter(text, false)
    }

    /// Queue filtered paste and an optional following Enter atomically.
    pub fn paste_with_enter(&mut self, text: &str, enter: bool) -> Result<(), TerminalError> {
        let bracketed = self.bracketed_paste();
        let filtered = crate::paste::filter(text, bracketed);

        if bracketed || enter {
            let mut payload = Vec::with_capacity(
                filtered.len() + BRACKETED_PASTE_WRAP_OVERHEAD + usize::from(enter),
            );
            if bracketed {
                payload.extend_from_slice(BRACKETED_PASTE_START);
            }
            payload.extend_from_slice(filtered.as_bytes());
            if bracketed {
                payload.extend_from_slice(BRACKETED_PASTE_END);
            }
            if enter {
                payload.push(b'\r');
            }
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

    /// Drain completions recorded while parsing PTY output, once per execution.
    /// Consume after [`Self::process_input`] on the existing PTY wake path.
    pub fn drain_command_completions(&mut self) -> impl Iterator<Item = CommandCompletion> + '_ {
        self.shell_integration.drain_completions()
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

    /// Kitty keyboard flags negotiated by the foreground application.
    pub fn keyboard_modes(&self) -> KeyboardModes {
        (*self.term.mode()).into()
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

/// Event state borrowed independently of the parser and terminal grid.
struct ParserEvents<'a> {
    proxy: &'a EventProxy,
    shell: &'a mut ShellIntegration,
    title: &'a mut String,
    pending_exit: &'a mut Option<Option<std::process::ExitStatus>>,
    exit_delivered: bool,
    replies: &'a mut Vec<u8>,
    size: TerminalSize,
    theme: &'a Theme,
    clipboard: &'a mut ClipboardRequests,
}

impl ParserEvents<'_> {
    fn drain(&mut self, term: &mut Term<EventProxy>) {
        for event in self.proxy.drain() {
            match event {
                TerminalEvent::ShellIntegration(AlacrittyEvent::ShellIntegration(
                    params,
                    point,
                    scroll,
                    generation,
                )) => self.shell.marker(&params, point, scroll, generation),
                TerminalEvent::ShellIntegration(AlacrittyEvent::ShellErase(
                    point,
                    mode,
                    scroll,
                    generation,
                )) => self.shell.erase(point, mode, scroll, generation),
                TerminalEvent::ShellIntegration(AlacrittyEvent::ShellCellsChanged(
                    start,
                    end,
                    scroll,
                    generation,
                )) => self.shell.cells_changed(start, end, scroll, generation),
                TerminalEvent::TitleChanged(title) => *self.title = title,
                TerminalEvent::TitleReset => self.title.clear(),
                TerminalEvent::Exit(status) if !self.exit_delivered => {
                    self.pending_exit.get_or_insert(status);
                }
                TerminalEvent::PtyWrite(bytes) | TerminalEvent::ClipboardDeniedReply(bytes) => {
                    self.replies.extend_from_slice(&bytes);
                }
                TerminalEvent::Query(AlacrittyEvent::ColorRequest(index, formatter)) => {
                    if index < alacritty_terminal::term::color::COUNT
                        && let Some(color) =
                            term.colors()[index].or_else(|| theme_color(self.theme, index))
                    {
                        self.replies.extend_from_slice(formatter(color).as_bytes());
                    }
                }
                TerminalEvent::Query(AlacrittyEvent::TextAreaSizeRequest(formatter)) => {
                    // The library formatter multiplies u16 dimensions. Clamp
                    // cell sizes so unusually large viewports cannot overflow.
                    let mut size = self.size.to_window_size();
                    size.cell_width = size.cell_width.min(u16::MAX / size.num_cols.max(1));
                    size.cell_height = size.cell_height.min(u16::MAX / size.num_lines.max(1));
                    self.replies.extend_from_slice(formatter(size).as_bytes());
                }
                TerminalEvent::Clipboard(request) => {
                    if let Err(ClipboardRequest::Load { formatter, .. }) =
                        self.clipboard.push(request)
                    {
                        // Bounded overflow denies the read with an empty response.
                        self.replies.extend_from_slice(formatter("").as_bytes());
                    }
                }
                _ => {}
            }
        }
        let (scroll, generation) = term.shell_coordinates();
        let oldest = scroll.min(i64::MAX as u64) as i64 + term.grid().topmost_line().0 as i64;
        // The alternate grid has no history; its bounds cannot evict main-grid markers.
        if !term.mode().contains(alacritty_terminal::term::TermMode::ALT_SCREEN) {
            self.shell.synchronize(generation, oldest);
        }
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
        let term = Term::new(
            TermConfig {
                kitty_keyboard: true,
                osc52: alacritty_terminal::term::Osc52::CopyPaste,
                ..TermConfig::default()
            },
            &dimensions,
            event_proxy.clone(),
        );
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
                clipboard_requests: ClipboardRequests::default(),
            },
            peer,
        )
    }

    fn parse_one_chunk(terminal: &mut Terminal) -> ProcessOutcome {
        terminal.process_input_with_budget(Instant::now(), Duration::ZERO, PARSE_BYTE_BUDGET)
    }

    #[test]
    fn synchronized_update_normal_end_and_all_chunk_splits() {
        let payload = b"\x1b[?2026h\x1b]2;released\x07hello\x1b[?2026l";
        for split in 0..=payload.len() {
            let (mut terminal, _) = buffered_terminal();
            terminal.parse_chunk(&payload[..split]);
            terminal.parse_chunk(&payload[split..]);
            assert_eq!(terminal.title(), "released", "split {split}");
            assert!(terminal.sync_deadline().is_none(), "split {split}");
            assert_eq!(terminal.grid()[GridLine(0)][GridColumn(0)].c, 'h');
        }
        let (mut terminal, _) = buffered_terminal();
        terminal.parse_chunk(b"\x1b[?2026h\x1b]2;released\x07hello");
        assert!(terminal.title().is_empty());
        assert_eq!(terminal.grid()[GridLine(0)][GridColumn(0)].c, ' ');
        assert!(terminal.sync_deadline().is_some());
        terminal.parse_chunk(b"\x1b[?2026l");
        assert_eq!(terminal.title(), "released");
        assert!(terminal.sync_deadline().is_none());
    }

    #[test]
    fn synchronized_update_expiry_releases_output_once_without_more_input() {
        let (mut terminal, peer) = buffered_terminal();
        peer.send(b"\x1b[?2026h\x1b]2;expired\x07\x1b[6nhello".to_vec());
        terminal.process_input();
        let deadline = terminal.sync_deadline().unwrap();
        assert!(!terminal.expire_synchronized_update(deadline - Duration::from_nanos(1)));
        assert!(terminal.title().is_empty());
        assert!(terminal.expire_synchronized_update(deadline));
        assert_eq!(terminal.title(), "expired");
        assert_eq!(peer.reply(), b"\x1b[1;1R");
        assert!(terminal.sync_deadline().is_none());
        assert!(!terminal.expire_synchronized_update(deadline + Duration::from_secs(1)));
        assert!(!terminal.process_input().grid_maybe_changed);
    }

    #[test]
    fn synchronized_update_process_input_expires_without_a_pty_wake() {
        let (mut terminal, peer) = buffered_terminal();
        peer.send(b"\x1b[?2026h\x1b]2;expired\x07".to_vec());
        terminal.process_input();
        let deadline = terminal.sync_deadline().unwrap();
        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        let expired = terminal.process_input();
        assert!(expired.grid_maybe_changed);
        assert!(!expired.more_output);
        assert_eq!(terminal.title(), "expired");
        assert!(terminal.sync_deadline().is_none());
    }

    #[test]
    fn synchronized_update_extension_and_end_then_restart_reset_deadline() {
        let (mut terminal, _) = buffered_terminal();
        terminal.parse_chunk(b"\x1b[?2026h\x1b]2;first\x07");
        let first = terminal.sync_deadline().unwrap();
        std::thread::sleep(Duration::from_millis(1));
        terminal.parse_chunk(b"\x1b[?2026h");
        let extended = terminal.sync_deadline().unwrap();
        assert!(extended > first);
        assert!(!terminal.expire_synchronized_update(first));
        // Completing the old update and beginning a new one in the same chunk
        // must release only the old output and retain the new deadline.
        terminal.parse_chunk(b"\x1b[?2026l\x1b[?2026h\x1b]2;second\x07");
        assert_eq!(terminal.title(), "first");
        let second = terminal.sync_deadline().unwrap();
        assert!(terminal.expire_synchronized_update(second));
        assert_eq!(terminal.title(), "second");
        assert!(terminal.sync_deadline().is_none());
        // Shell restart constructs a fresh terminal rather than carrying the
        // previous child's parser state or timeout into the new shell.
        terminal.parse_chunk(b"\x1b[?2026h\x1b]2;old-child\x07");
        let (restarted, _) = buffered_terminal();
        assert!(restarted.sync_deadline().is_none());
        assert!(restarted.title().is_empty());
    }

    #[test]
    fn synchronized_update_flushes_before_exit_or_transport_failure() {
        use std::os::unix::process::ExitStatusExt as _;
        for failed in [false, true] {
            let (mut terminal, peer) = buffered_terminal();
            peer.send(b"\x1b[?2026h\x1b]2;final\x07hello\x1b[6n".to_vec());
            let status = std::process::ExitStatus::from_raw(0);
            if failed {
                peer.fail();
                peer.finish();
            } else {
                peer.exit(Some(status));
            }
            let budgeted = parse_one_chunk(&mut terminal);
            assert!(budgeted.more_output);
            assert!(budgeted.child_exit.is_none() && budgeted.io_error.is_none());
            assert!(terminal.title().is_empty());
            let done = terminal.process_input();
            assert!(done.grid_maybe_changed);
            assert_eq!(terminal.title(), "final");
            assert!(terminal.sync_deadline().is_none());
            if failed {
                assert_eq!(done.io_error.as_deref(), Some("test transport failure"));
            } else {
                assert_eq!(peer.reply(), b"\x1b[1;6R");
                assert_eq!(done.child_exit, Some(Some(status)));
            }
        }
    }

    #[test]
    fn synchronized_update_expiry_preserves_incomplete_utf8_and_csi() {
        let (mut terminal, _) = buffered_terminal();
        terminal.parse_chunk(b"\x1b[?2026h\xe2");
        assert!(terminal.expire_synchronized_update(terminal.sync_deadline().unwrap()));
        terminal.parse_chunk(b"\x82\xac\x1b[");
        terminal.parse_chunk(b"31mX");
        assert_eq!(terminal.grid()[GridLine(0)][GridColumn(0)].c, '€');
        let x = &terminal.grid()[GridLine(0)][GridColumn(1)];
        assert_eq!(x.c, 'X');
        assert_eq!(
            x.fg,
            alacritty_terminal::vte::ansi::Color::Named(
                alacritty_terminal::vte::ansi::NamedColor::Red
            )
        );
    }

    #[test]
    fn synchronized_update_color_queries_observe_palette_at_each_osc() {
        let body = b"\x1b]10;?\x1b\\\x1b]10;#112233\x07\x1b]10;?\x07\x1b]110\x07\x1b]10;?\x1b\\";
        let expected = b"\x1b]10;rgb:5252/e8e8/ffff\x1b\\\x1b]10;rgb:1111/2222/3333\x07\x1b]10;rgb:5252/e8e8/ffff\x1b\\";
        for expire in [false, true] {
            for split in 0..=body.len() {
                let (mut terminal, peer) = buffered_terminal();
                terminal.parse_chunk(b"\x1b[?2026h");
                terminal.parse_chunk(&body[..split]);
                terminal.parse_chunk(&body[split..]);
                if expire {
                    assert!(terminal.expire_synchronized_update(terminal.sync_deadline().unwrap()));
                } else {
                    terminal.parse_chunk(b"\x1b[?2026l");
                }
                assert_eq!(peer.reply(), expected, "expire {expire}, split {split}");
            }
        }
    }

    #[test]
    fn synchronized_update_shell_and_clipboard_events_preserve_source_order() {
        let (mut terminal, peer) = buffered_terminal();
        terminal.parse_chunk(b"\x1b[?2026h\x1b]133;A\x07\x1b]133;C\x07output\r\n\x1b]133;D;7\x07\x1b]52;c;b25l\x07\x1b]52;c;?\x1b\\\x1b]52;c;dHdv\x07\x1b]52;c;?\x07\x1b]10;?\x07\x1b]10;#112233\x07");
        assert!(terminal.drain_command_completions().next().is_none());
        assert!(terminal.drain_clipboard_requests().is_empty());
        assert!(terminal.expire_synchronized_update(terminal.sync_deadline().unwrap()));
        let completions = terminal.drain_command_completions().collect::<Vec<_>>();
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].exit_status, Some(7));
        assert_eq!(terminal.last_command_output().as_deref(), Some("output"));
        let requests = terminal.drain_clipboard_requests();
        assert_eq!(requests.len(), 4);
        assert!(matches!(&requests[0], ClipboardRequest::Store { text, .. } if text == "one"));
        assert!(matches!(&requests[1], ClipboardRequest::Load { .. }));
        assert!(matches!(&requests[2], ClipboardRequest::Store { text, .. } if text == "two"));
        assert!(matches!(&requests[3], ClipboardRequest::Load { .. }));
        assert_eq!(peer.reply(), b"\x1b]10;rgb:5252/e8e8/ffff\x07");
        assert!(terminal.drain_clipboard_requests().is_empty());
    }

    #[test]
    fn clipboard_protocol_reply_preserves_viewport_and_overflow_denies_reads() {
        let (mut terminal, peer) = buffered_terminal();
        terminal.inject_local(&b"line\r\n".repeat(50));
        terminal.term.scroll_display(Scroll::Delta(5));
        let offset = terminal.grid().display_offset();
        terminal.write_clipboard_reply(b"raw-reply").unwrap();
        assert_eq!(peer.reply(), b"raw-reply");
        assert_eq!(terminal.grid().display_offset(), offset);
        terminal.parse_chunk(&b"\x1b]52;c;?\x07".repeat(17));
        assert_eq!(terminal.drain_clipboard_requests().len(), 16);
        assert_eq!(peer.reply(), b"\x1b]52;c;\x07");
        assert_eq!(terminal.grid().display_offset(), offset);
    }

    fn clipboard_x_payload(decoded_bytes: usize, terminator: &[u8]) -> Vec<u8> {
        // The fixture encodes ASCII 'x' without adding a test-only dependency.
        let mut payload = b"\x1b]52;c;".to_vec();
        payload.extend_from_slice(&b"eHh4".repeat(decoded_bytes / 3));
        payload.extend_from_slice(match decoded_bytes % 3 {
            1 => b"eA==",
            2 => b"eHg=",
            _ => b"",
        });
        payload.extend_from_slice(terminator);
        payload
    }

    #[test]
    fn clipboard_osc52_decoded_boundary_accepts_complete_bel_and_st() {
        use crate::clipboard::MAX_CLIPBOARD_BYTES;
        for terminator in [b"\x07".as_slice(), b"\x1b\\".as_slice()] {
            for decoded_bytes in
                [MAX_CLIPBOARD_BYTES - 1, MAX_CLIPBOARD_BYTES, MAX_CLIPBOARD_BYTES + 1]
            {
                let (mut terminal, _) = buffered_terminal();
                let payload = clipboard_x_payload(decoded_bytes, terminator);
                // Feed through the normal PTY-sized parser slices, with ST's
                // ESC/backslash deliberately split on the final two calls.
                let prefix = payload.len() - terminator.len();
                for chunk in payload[..prefix].chunks(64 * 1024) {
                    terminal.parse_chunk(chunk);
                    assert!(terminal.drain_clipboard_requests().is_empty());
                }
                for (index, byte) in terminator.iter().enumerate() {
                    terminal.parse_chunk(&[*byte]);
                    if index + 1 != terminator.len() {
                        assert!(terminal.drain_clipboard_requests().is_empty());
                    }
                }
                let requests = terminal.drain_clipboard_requests();
                if decoded_bytes <= MAX_CLIPBOARD_BYTES {
                    assert_eq!(requests.len(), 1);
                    assert!(matches!(&requests[0], ClipboardRequest::Store { text, .. }
                        if text.len() == decoded_bytes && text.bytes().all(|byte| byte == b'x')));
                } else {
                    assert!(requests.is_empty());
                }
                terminal.parse_chunk(b"\x1b]52;c;b25l\x07");
                assert!(matches!(&terminal.drain_clipboard_requests()[0],
                    ClipboardRequest::Store { text, .. } if text == "one"));
            }
        }
    }

    #[test]
    fn clipboard_osc52_oversized_unterminated_payload_is_discarded_and_recovers() {
        use crate::clipboard::MAX_CLIPBOARD_BYTES;
        for ending in [
            b"\x07".as_slice(),
            b"\x1b\\".as_slice(),
            b"\x18".as_slice(),
            b"\x1a".as_slice(),
            b"".as_slice(),
        ] {
            let (mut terminal, _) = buffered_terminal();
            let payload = clipboard_x_payload(MAX_CLIPBOARD_BYTES + 1024, b"");
            for chunk in payload.chunks(64 * 1024) {
                terminal.parse_chunk(chunk);
                assert!(terminal.drain_clipboard_requests().is_empty());
            }
            terminal.parse_chunk(ending);
            assert!(terminal.drain_clipboard_requests().is_empty());
            // Starting another OSC must discard the oversized unfinished one
            // as well as recovering from ordinary termination or cancellation.
            terminal.parse_chunk(b"\x1b]52;c;dHdv\x1b");
            assert!(terminal.drain_clipboard_requests().is_empty());
            terminal.parse_chunk(b"\\");
            let requests = terminal.drain_clipboard_requests();
            assert_eq!(requests.len(), 1);
            assert!(matches!(&requests[0], ClipboardRequest::Store { text, .. } if text == "two"));
        }
    }

    #[test]
    fn clipboard_osc52_cancelled_or_malformed_sequences_never_publish_partial_requests() {
        let rejected = [
            b"\x1b]52;c;b25l\x18".as_slice(),
            b"\x1b]52;c;b25l\x1a".as_slice(),
            b"\x1b]52;c;b25l\x1bX".as_slice(),
            b"\x1b]52;c;b25l\x1b\x18".as_slice(),
            b"\x1b]52;c;b25l\x1b\x1a".as_slice(),
            b"\x1b]52;c;b25l;garbage\x07".as_slice(),
            b"\x1b]52;c;b25l;;;;;;;;;;;;;;;;;;\x07".as_slice(),
            b"\x1b]52;c;?\x18".as_slice(),
            b"\x1b]52;c;?\x1a".as_slice(),
            b"\x1b]52;c;?\x1bX".as_slice(),
            b"\x1b]52;c;invalid-base64\x07".as_slice(),
            // Valid base64 containing a byte that is not valid UTF-8.
            b"\x1b]52;c;/w==\x07".as_slice(),
        ];
        for (case, payload) in rejected.into_iter().enumerate() {
            for split in 0..=payload.len() {
                let (mut terminal, _) = buffered_terminal();
                terminal.parse_chunk(&payload[..split]);
                assert!(terminal.drain_clipboard_requests().is_empty());
                terminal.parse_chunk(&payload[split..]);
                assert!(
                    terminal.drain_clipboard_requests().is_empty(),
                    "case {case}, split {split}"
                );
                terminal.parse_chunk(b"\x1b]52;c;b25l\x07");
                let requests = terminal.drain_clipboard_requests();
                assert_eq!(requests.len(), 1, "recovery case {case}, split {split}");
                assert!(
                    matches!(&requests[0], ClipboardRequest::Store { text, .. } if text == "one")
                );
            }
        }
    }

    #[test]
    fn kitty_keyboard_negotiation_queries_active_flags_and_restores_stack() {
        let (mut terminal, peer) = buffered_terminal();
        assert!(terminal.keyboard_modes().is_empty());
        for (sequence, bits) in [
            (b"\x1b[>1u".as_slice(), 1),
            (b"\x1b[=2;2u".as_slice(), 3),
            (b"\x1b[>31u".as_slice(), 31),
            (b"\x1b[<u".as_slice(), 3),
            (b"\x1b[=1;3u".as_slice(), 2),
            (b"\x1b[=0u".as_slice(), 0),
            (b"\x1b[<4097u".as_slice(), 0),
        ] {
            terminal.parse_chunk(sequence);
            assert_eq!(terminal.keyboard_modes().bits(), bits);
            terminal.parse_chunk(b"\x1b[?u");
            assert_eq!(peer.reply(), format!("\x1b[?{bits}u").as_bytes());
        }
    }

    #[test]
    fn kitty_keyboard_modes_follow_alternate_screen_and_reset() {
        let (mut terminal, peer) = buffered_terminal();
        for (sequence, bits) in [
            (b"\x1b[=3u".as_slice(), 3),
            (b"\x1b[?1049h".as_slice(), 0),
            (b"\x1b[>8u\x1b[=16;2u".as_slice(), 24),
            (b"\x1b[?1049l".as_slice(), 3),
            (b"\x1b[?1049h".as_slice(), 24),
            (b"\x1b[?1049l\x1bc".as_slice(), 0),
        ] {
            terminal.parse_chunk(sequence);
            assert_eq!(terminal.keyboard_modes().bits(), bits);
            terminal.parse_chunk(b"\x1b[?u");
            assert_eq!(peer.reply(), format!("\x1b[?{bits}u").as_bytes());
        }
    }

    #[test]
    fn kitty_keyboard_hostile_pushes_are_bounded_and_preserve_saved_title() {
        for title in [false, true] {
            let (mut terminal, _) = buffered_terminal();
            if title {
                terminal.parse_chunk(b"\x1b]2;saved-title\x07\x1b[22t\x1b]2;new-title\x07");
            }
            terminal.parse_chunk(&b"\x1b[>31u".repeat(4097));
            assert_eq!(terminal.keyboard_modes().bits(), 31);
            terminal.parse_chunk(b"\x1b[<4095u");
            assert_eq!(terminal.keyboard_modes().bits(), 31);
            terminal.parse_chunk(b"\x1b[<u");
            assert!(terminal.keyboard_modes().is_empty());
            if title {
                terminal.parse_chunk(b"\x1b[23t");
                assert_eq!(terminal.title(), "saved-title");
            } else {
                assert!(terminal.title().is_empty());
            }
        }
    }

    #[test]
    #[ignore = "parser throughput benchmark; run alone with --ignored --nocapture"]
    fn synchronized_parser_benchmark() {
        let ascii = b"plain terminal output without queries\r\n".repeat(1600);
        let title = b"\x1b]2;dense-title\x07".repeat(3600);
        let queries = b"\x1b]10;?\x07\x1b]10;#112233\x07\x1b]10;?\x07\x1b]110\x07".repeat(1200);
        for (name, payload) in [("ascii", ascii), ("title", title), ("queries", queries)] {
            for mode in ["plain", "timeout", "end"] {
                let (mut terminal, peer) = buffered_terminal();
                let rounds = 128;
                let warmups = 16;
                let mut started = Instant::now();
                for iteration in 0..warmups + rounds {
                    if iteration == warmups {
                        started = Instant::now();
                    }
                    if mode != "plain" {
                        terminal.parse_chunk(b"\x1b[?2026h");
                    }
                    terminal.parse_chunk(&payload);
                    if mode == "timeout" {
                        terminal.expire_synchronized_update(terminal.sync_deadline().unwrap());
                    } else if mode == "end" {
                        terminal.parse_chunk(b"\x1b[?2026l");
                    }
                    if name == "queries" {
                        let _ = peer.reply();
                    }
                }
                let elapsed = started.elapsed();
                assert!(terminal.sync_deadline().is_none());
                eprintln!(
                    "parser,{name},mode={mode},bytes={},elapsed_ns={},MiB_per_second={:.2}",
                    payload.len() * rounds,
                    elapsed.as_nanos(),
                    (payload.len() * rounds) as f64 / elapsed.as_secs_f64() / (1024.0 * 1024.0)
                );
            }
        }
    }

    #[test]
    fn shell_completions_drain_all_fast_commands_in_one_input_turn() {
        let (mut terminal, peer) = buffered_terminal();
        peer.send(
            b"\x1b]7;file:///tmp/a%20b\x07\x1b]133;A\x07\x1b]133;C\x07\x1b]133;D;0\x07\x1b]133;A\x07\x1b]133;C\x07\x1b]133;D;130\x07\x1b]133;D;0\x07".to_vec(),
        );
        let outcome = terminal.process_input();
        assert!(outcome.grid_maybe_changed);
        assert!(!terminal.shell_integration().is_running());
        let completions = terminal.drain_command_completions().collect::<Vec<_>>();
        assert_eq!(completions.iter().map(|c| c.id).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(
            completions.iter().map(|c| c.exit_status).collect::<Vec<_>>(),
            [Some(0), Some(130)]
        );
        assert!(completions.iter().all(|c| c.cwd.as_deref() == Some("/tmp/a b")));
        assert!(terminal.drain_command_completions().next().is_none());
        terminal.process_input();
        assert!(terminal.drain_command_completions().next().is_none());
    }

    #[test]
    fn shell_completion_survives_clear_and_real_grid_reflow() {
        let (mut terminal, _) = buffered_terminal();
        terminal.parse_chunk(b"\x1b]133;A\x07\x1b]133;C\x07output");
        // Exercise the same grid reflow as resize without requiring a live
        // descriptor from the buffered test transport.
        terminal.shell_integration.invalidate();
        terminal.term.resize(TermDimensions { columns: 40, screen_lines: 24 });
        terminal.size.columns = 40;
        terminal.parse_chunk(b"\x1b[2J\x1b]133;D;7\x07");
        assert!(terminal.shell_integration().commands().is_empty());
        let completion = terminal.drain_command_completions().next().unwrap();
        assert_eq!(completion.id, 1);
        assert_eq!(completion.exit_status, Some(7));
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
    fn paste_with_enter_keeps_bracket_end_before_enter_in_one_queue_slot() {
        let (mut terminal, peer) = buffered_terminal();
        peer.send(b"\x1b[?2004h".to_vec());
        terminal.process_input();
        assert!(terminal.bracketed_paste());
        // Only one of the transport's 1024 message slots remains. Paste and
        // Enter must both fit, without executing Enter inside bracketed text.
        for _ in 0..1023 {
            terminal.write_to_pty(b"x").unwrap();
        }
        terminal.paste_with_enter("日本語\r\n\x1b[201~tail", true).unwrap();
        for _ in 0..1023 {
            assert_eq!(peer.reply(), b"x");
        }
        assert_eq!(peer.reply(), "\x1b[200~日本語\ntail\x1b[201~\r".as_bytes());
    }

    #[test]
    fn paste_with_enter_rejects_the_whole_payload_at_the_input_byte_limit() {
        let (mut terminal, peer) = buffered_terminal();
        peer.send(b"\x1b[?2004h".to_vec());
        terminal.process_input();
        // Leave room for the framed paste, but not for its following Enter.
        // A split enqueue would accept text before rejecting Enter, making a
        // client's retry duplicate input or accidentally execute old text.
        let filler = vec![b'x'; 8 * 1024 * 1024 - b"\x1b[200~run\x1b[201~".len()];
        terminal.write_to_pty(&filler).unwrap();
        assert!(
            matches!(terminal.paste_with_enter("run", true), Err(TerminalError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock)
        );
        assert_eq!(peer.reply(), filler);
        terminal.write_to_pty(b"TAIL").unwrap();
        // Any accepted paste fragment would precede TAIL in the queue.
        assert_eq!(peer.reply(), b"TAIL");
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
