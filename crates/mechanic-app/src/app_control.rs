//! Main-thread control dispatch and bounded completion waiters.
use super::*;
use crate::control::{
    Completion, ControlEvent, ControlServer, ErrorCode, InputMode, Operation, PaneInfo, Response,
    ResponseResult, SessionSelector,
};
use std::sync::mpsc::SyncSender;

pub(super) const MAX_COMPLETIONS: usize = 1024;
const MAX_WAITERS: usize = crate::control::MAX_CONNECTIONS;

#[derive(Default)]
pub(super) struct ControlService {
    pub(super) server: Option<ControlServer>,
    waiters: Vec<Waiter>,
}

struct Waiter {
    session: SessionSelector,
    after: u64,
    deadline: Instant,
    reply: SyncSender<Response>,
}

impl ControlService {
    fn instance_id(&self) -> &str {
        self.server.as_ref().map_or("", ControlServer::instance_id)
    }

    pub(super) fn complete(
        &mut self,
        session_id: u64,
        completion: &mechanic_core::CommandCompletion,
    ) {
        if self.waiters.is_empty() {
            return;
        }
        let instance = self.instance_id().to_owned();
        self.waiters.retain(|waiter| {
            if waiter.session.session_id == session_id && completion.id > waiter.after {
                let response = if Instant::now() >= waiter.deadline {
                    ResponseResult::Timeout { session: waiter.session.clone() }
                } else {
                    ResponseResult::Completed {
                        completion: completion_info(&waiter.session, completion),
                    }
                };
                let _ = waiter.reply.try_send(Response::new(&instance, response));
                false
            } else {
                true
            }
        });
    }

    pub(super) fn retire(&mut self, session_id: u64, code: ErrorCode) {
        let instance = self.instance_id().to_owned();
        self.waiters.retain(|waiter| {
            if waiter.session.session_id == session_id {
                let _ = waiter.reply.try_send(Response::error(
                    &instance,
                    code,
                    "terminal session ended",
                ));
                false
            } else {
                true
            }
        });
    }

    pub(super) fn close_all(&mut self) {
        let instance = self.instance_id().to_owned();
        for waiter in self.waiters.drain(..) {
            let _ = waiter.reply.try_send(Response::error(
                &instance,
                ErrorCode::Closed,
                "application exited",
            ));
        }
    }

    pub(super) fn expire(&mut self, now: Instant, earliest: &mut Option<Instant>) {
        let instance = self.instance_id().to_owned();
        self.waiters.retain(|waiter| {
            if now >= waiter.deadline {
                let _ = waiter.reply.try_send(Response::new(
                    &instance,
                    ResponseResult::Timeout { session: waiter.session.clone() },
                ));
                false
            } else {
                merge_deadline(earliest, waiter.deadline);
                true
            }
        });
    }
}

impl App {
    pub(super) fn dispatch_control(&mut self, event: ControlEvent) {
        let instance = self.control_service.instance_id().to_owned();
        let error = |code, message: &str| {
            let _ = event.reply.try_send(Response::error(&instance, code, message));
        };
        if self.control_service.server.is_none() {
            error(ErrorCode::Unsupported, "local control is disabled");
            return;
        }
        if let Err((code, message)) = event.request.validate(&instance) {
            error(code, message);
            return;
        }
        if Instant::now() >= event.deadline {
            error(ErrorCode::Deadline, "request expired before dispatch");
            return;
        }
        match event.request.operation {
            Operation::Ping => {
                let _ = event.reply.try_send(Response::new(instance, ResponseResult::Pong));
            }
            Operation::ListPanes => {
                let mut panes = Vec::new();
                for (window_id, state) in &self.windows {
                    for pane_id in state.tree.pane_ids() {
                        let Some(pane) = state.pane_state(pane_id) else { continue };
                        let shell = pane.terminal.shell_integration();
                        panes.push(PaneInfo {
                            session: SessionSelector {
                                instance_id: instance.clone(),
                                session_id: pane.session,
                            },
                            window_id: format!("{window_id:?}"),
                            pane_id,
                            title: pane.terminal.title().to_owned(),
                            cwd: shell.cwd().map(str::to_owned).or_else(|| {
                                pane.directory
                                    .as_ref()
                                    .map(|directory| directory.to_string_lossy().into_owned())
                            }),
                            focused: state.focused && state.tree.active() == pane_id,
                            active: state.tree.active() == pane_id,
                            running: pane.exit_status.is_none() && shell.is_running(),
                            rows: pane.terminal.screen_lines(),
                            columns: pane.terminal.columns(),
                            shell_integration: shell.cwd().is_some()
                                || !shell.commands().is_empty(),
                            latest_command_id: pane
                                .completions
                                .back()
                                .map_or(0, |completion| completion.id),
                            last_command: None,
                            last_status: shell.last_exit_status(),
                        });
                    }
                }
                panes.sort_by_key(|pane| pane.session.session_id);
                let _ =
                    event.reply.try_send(Response::new(instance, ResponseResult::Panes { panes }));
            }
            operation => {
                let session = operation.session().expect("targeted operation").clone();
                let target = self.windows.iter().find_map(|(id, state)| {
                    state
                        .tree
                        .pane_ids()
                        .into_iter()
                        .find(|pane_id| {
                            state
                                .pane_state(*pane_id)
                                .is_some_and(|pane| pane.session == session.session_id)
                        })
                        .map(|pane| (*id, pane))
                });
                let Some((window_id, pane_id)) = target else {
                    error(ErrorCode::UnknownSession, "terminal session is absent or was restarted");
                    return;
                };
                let state = self.windows.get_mut(&window_id).expect("target window");
                state.load_pane(pane_id);
                match operation {
                    Operation::ReadOutput { max_lines, max_bytes, .. } => {
                        let (text, truncated) =
                            read_grid(state.pane.terminal.grid(), max_lines, max_bytes);
                        let _ = event.reply.try_send(Response::new(
                            &instance,
                            ResponseResult::Output { session, text, truncated },
                        ));
                    }
                    Operation::SendInput { text, mode, enter, .. } => {
                        if Instant::now() >= event.deadline {
                            error(ErrorCode::Deadline, "request expired before input dispatch");
                        } else if state.pane.exit_status.is_some() {
                            error(ErrorCode::Closed, "terminal session is no longer writable");
                        } else {
                            let display_offset = state.pane.terminal.grid().display_offset();
                            let result = match mode {
                                InputMode::Paste => {
                                    state.pane.terminal.paste_with_enter(&text, enter)
                                }
                                InputMode::Raw => {
                                    let mut payload = text.into_bytes();
                                    if enter {
                                        payload.push(b'\r');
                                    }
                                    state.pane.terminal.write_to_pty(&payload)
                                }
                            };
                            // Input returns the viewport to the live screen
                            // before queuing the PTY write. A silent child (or
                            // failed write) may produce no output wake to redraw it.
                            if state.pane.terminal.grid().display_offset() != display_offset {
                                state.mark_content_dirty();
                                state.request_redraw();
                            }
                            match result {
                                Ok(()) => {
                                    let _ = event.reply.try_send(Response::new(
                                        &instance,
                                        ResponseResult::InputSent { session },
                                    ));
                                }
                                Err(error_message) => {
                                    error(ErrorCode::Internal, &error_message.to_string())
                                }
                            }
                        }
                    }
                    Operation::Wait { after_command_id, timeout_ms, .. } => {
                        if let Some(completion) = state
                            .pane
                            .completions
                            .iter()
                            .find(|completion| completion.id > after_command_id)
                        {
                            let _ = event.reply.try_send(Response::new(
                                &instance,
                                ResponseResult::Completed {
                                    completion: completion_info(&session, completion),
                                },
                            ));
                        } else if state.pane.exit_status.is_some() {
                            error(ErrorCode::Closed, "terminal session has exited");
                        } else if timeout_ms == 0 {
                            let _ = event.reply.try_send(Response::new(
                                &instance,
                                ResponseResult::Timeout { session },
                            ));
                        } else if self.control_service.waiters.len() >= MAX_WAITERS {
                            error(ErrorCode::Busy, "completion waiter limit reached");
                        } else {
                            let deadline = (Instant::now() + Duration::from_millis(timeout_ms))
                                .min(event.deadline);
                            self.control_service.waiters.push(Waiter {
                                session,
                                after: after_command_id,
                                deadline,
                                reply: event.reply,
                            });
                        }
                    }
                    _ => unreachable!(),
                }
                state.load_pane(state.tree.active());
            }
        }
    }
}

fn completion_info(
    session: &SessionSelector,
    completion: &mechanic_core::CommandCompletion,
) -> Completion {
    Completion {
        session: session.clone(),
        command_id: completion.id,
        exit_status: completion.exit_status,
        command: None,
        cwd: completion.cwd.clone(),
    }
}

/// Return recent original cell text from the active grid and retained history.
/// Read physical rows without introducing a newline at soft wraps; wide spacers
/// are omitted and combining marks retain their stored spelling and order.
fn read_grid(
    grid: &alacritty_terminal::Grid<alacritty_terminal::term::cell::Cell>,
    max_lines: usize,
    max_bytes: usize,
) -> (String, bool) {
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line, Point};
    use alacritty_terminal::term::cell::Flags;
    let columns = grid.columns();
    if columns == 0 || max_lines == 0 || max_bytes == 0 {
        return (String::new(), false);
    }
    let scan_budget = max_bytes.saturating_mul(8).max(columns);
    let rows = grid.total_lines().min(max_lines).min(scan_budget / columns);
    let mut truncated = rows < grid.total_lines();
    let first = grid.screen_lines() as i32 - rows as i32;
    let last = grid.screen_lines() as i32 - 1;
    let mut reversed = Vec::with_capacity(max_bytes.min(4096));
    let mut bytes = 0;
    'rows: for line in (first..=last).rev() {
        let wrapped =
            grid[Point::new(Line(line), Column(columns - 1))].flags.contains(Flags::WRAPLINE);
        if !wrapped && line != last {
            if bytes == max_bytes {
                truncated = true;
                break;
            }
            reversed.push('\n');
            bytes += 1;
        }
        let mut trim_spaces = !wrapped;
        for column in (0..columns).rev() {
            let cell = &grid[Point::new(Line(line), Column(column))];
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let marks = cell.zerowidth().unwrap_or_default();
            if trim_spaces && cell.c == ' ' && marks.is_empty() {
                continue;
            }
            trim_spaces = false;
            for character in marks.iter().rev().copied().chain(std::iter::once(cell.c)) {
                if bytes + character.len_utf8() > max_bytes {
                    truncated = true;
                    break 'rows;
                }
                bytes += character.len_utf8();
                reversed.push(character);
            }
        }
    }
    (reversed.into_iter().rev().collect(), truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    struct Size {
        columns: usize,
        rows: usize,
    }
    impl Dimensions for Size {
        fn total_lines(&self) -> usize {
            self.rows
        }
        fn screen_lines(&self) -> usize {
            self.rows
        }
        fn columns(&self) -> usize {
            self.columns
        }
    }

    fn terminal(columns: usize, rows: usize, source: &str) -> Term<VoidListener> {
        let config = Config { scrolling_history: 100, ..Config::default() };
        let mut term = Term::new(config, &Size { columns, rows }, VoidListener);
        Processor::<StdSyncHandler>::new().advance(&mut term, source.as_bytes());
        term
    }

    #[test]
    fn read_preserves_unicode_spelling_and_softwraps_in_history() {
        for original in ["caf\u{e9} cafe\u{301}", "日本語 中文 한글", "العربية שלום", "Straße Σσς"]
        {
            let term = terminal(5, 1, original);
            let (output, truncated) = read_grid(term.grid(), 100, 4096);
            assert_eq!(output, original);
            assert!(!truncated);
        }
    }

    #[test]
    fn read_keeps_hard_breaks_and_selects_newest_rows() {
        let term = terminal(20, 1, "old\r\nnew\r\nlast");
        assert_eq!(read_grid(term.grid(), 3, 4096), ("old\nnew\nlast".into(), false));
        assert_eq!(read_grid(term.grid(), 2, 4096), ("new\nlast".into(), true));
    }

    #[test]
    fn read_byte_limit_retains_recent_unicode_without_splitting_utf8() {
        let term = terminal(20, 1, "前文日本語");
        let (output, truncated) = read_grid(term.grid(), 100, 7);
        assert_eq!(output, "本語");
        assert!(truncated);
        assert!(output.len() <= 7);
    }

    #[test]
    fn read_bounds_large_combining_cells_and_skips_wide_spacers() {
        let source = format!("中a{}", "\u{301}".repeat(100_000));
        let term = terminal(20, 1, &source);
        let (output, truncated) = read_grid(term.grid(), 100, 1024);
        assert_eq!(output, "\u{301}".repeat(512));
        assert!(truncated);
        let term = terminal(3, 1, "ab中文");
        assert_eq!(read_grid(term.grid(), 100, 1024), ("ab中文".into(), false));
    }

    #[test]
    fn every_completion_in_a_fast_batch_wakes_its_waiter() {
        let mut service = ControlService::default();
        let mut receivers = Vec::new();
        for after in [0, 1] {
            let (reply, receiver) = std::sync::mpsc::sync_channel(1);
            service.waiters.push(Waiter {
                session: SessionSelector { instance_id: "test".into(), session_id: 7 },
                after,
                deadline: Instant::now() + Duration::from_secs(1),
                reply,
            });
            receivers.push(receiver);
        }
        for id in [1, 2] {
            service.complete(
                7,
                &mechanic_core::CommandCompletion {
                    id,
                    duration: Duration::ZERO,
                    exit_status: Some(id as i32),
                    cwd: Some("/tmp".into()),
                    cwd_host: Some("localhost".into()),
                },
            );
        }
        for (receiver, expected) in receivers.iter().zip([1, 2]) {
            let ResponseResult::Completed { completion } = receiver.try_recv().unwrap().result
            else {
                panic!("missing completion")
            };
            assert_eq!(completion.command_id, expected);
        }
        assert!(service.waiters.is_empty());
    }

    #[test]
    fn waiter_retirement_and_timeout_clear_deadlines() {
        let mut service = ControlService::default();
        let (reply, receiver) = std::sync::mpsc::sync_channel(1);
        service.waiters.push(Waiter {
            session: SessionSelector { instance_id: "test".into(), session_id: 8 },
            after: 0,
            deadline: Instant::now(),
            reply,
        });
        let mut earliest = None;
        service.expire(Instant::now(), &mut earliest);
        assert!(matches!(receiver.try_recv().unwrap().result, ResponseResult::Timeout { .. }));
        assert!(earliest.is_none());
        let (reply, receiver) = std::sync::mpsc::sync_channel(1);
        service.waiters.push(Waiter {
            session: SessionSelector { instance_id: "test".into(), session_id: 9 },
            after: 0,
            deadline: Instant::now() + Duration::from_secs(1),
            reply,
        });
        service.retire(9, ErrorCode::Restarted);
        assert!(matches!(
            receiver.try_recv().unwrap().result,
            ResponseResult::Error { code: ErrorCode::Restarted, .. }
        ));
        assert!(service.waiters.is_empty());
    }
}
