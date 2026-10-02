//! Live PTY detachment smoke: the worker waits on real original reader wakes.
use super::*;
use crate::control::{ControlEvent, Operation, Request, Response, ResponseResult, SessionSelector};

pub(crate) struct LivePaneMoveFixture {
    socket: std::path::PathBuf,
    selector: SessionSelector,
    waiter: std::sync::mpsc::Receiver<Response>,
    expected_output: String,
    expected_window: String,
}

fn check(value: bool, message: &str) -> Result<(), String> {
    value.then_some(()).ok_or_else(|| message.to_owned())
}

impl App {
    pub(crate) fn smoke_prepare_live_detach(
        &mut self,
        events: &ActiveEventLoop,
    ) -> Result<(LivePaneMoveFixture, tempfile::TempDir), String> {
        let directory = tempfile::Builder::new()
            .prefix("mechanic-move-")
            .tempdir_in("/tmp")
            .map_err(|error| error.to_string())?;
        self.configure_services(false, None, Some(directory.path().join("ctl")));
        eprintln!("live move smoke: temporary control ready");
        let server =
            self.control_service.server.as_ref().ok_or("live move control server absent")?;
        let instance = server.instance_id().to_owned();
        let socket = server.socket_path().to_owned();
        let source = self.spawn_window(events, None).ok_or("live move source window failed")?;
        eprintln!("live move smoke: source window ready");
        let original_single = self.windows[&source].pane.session;
        let session_counter = self.next_session;
        let window_count = self.windows.len();
        check(
            self.detach_pane(events, source, 1, None) == Some(source),
            "single pane detach replaced its window",
        )?;
        check(
            self.next_session == session_counter
                && self.windows.len() == window_count
                && self.windows[&source].pane.session == original_single,
            "single pane detach restarted its shell",
        )?;
        let pane_id = self.split_pane(source, Axis::Vertical).ok_or("live move split failed")?;
        eprintln!("live move smoke: live split ready");
        let session = self.windows[&source].pane.session;
        let token = format!("surviving_session_{session}");
        {
            let state = self.windows.get_mut(&source).unwrap();
            state.load_pane(pane_id);
            state.pane.terminal.write_to_pty(format!(
                "mechanic_live_token={token}; printf 'READY_%s\\n' \"$mechanic_live_token\"\n"
            ).as_bytes()).map_err(|error| error.to_string())?;
        }
        self.live_move_wait_text(events, source, pane_id, &format!("READY_{token}"))?;
        eprintln!("live move smoke: original shell variable ready");
        let selection;
        {
            let state = self.windows.get_mut(&source).unwrap();
            state.load_pane(pane_id);
            state.pane.terminal.inject_local("LIVE_HISTORY_SENTINEL\r\n".repeat(100).as_bytes());
            state.pane.terminal.inject_local(b"LIVE_SELECTION_TOKEN");
            let line = state.pane.terminal.grid().cursor.point.line;
            state
                .pane
                .terminal
                .start_selection(GridPoint::new(line, GridColumn(0)), GridSide::Left);
            state
                .pane
                .terminal
                .update_selection(GridPoint::new(line, GridColumn(19)), GridSide::Right);
            selection = state.pane.terminal.selection_text();
            check(
                selection.as_deref() == Some("LIVE_SELECTION_TOKEN"),
                "live move selection setup failed",
            )?;
            state.pane.search.active = true;
            state.pane.search.set_query("LIVE_SELECTION_TOKEN".into(), &state.pane.terminal);
            state.pane.terminal.inject_local(b"\x1b]133;C\x07");
        }
        let selector = SessionSelector { instance_id: instance.clone(), session_id: session };
        let (reply, waiter) = std::sync::mpsc::sync_channel(1);
        self.dispatch_control(ControlEvent {
            request: Request::new(
                Some(instance.clone()),
                Operation::Wait {
                    session: selector.clone(),
                    after_command_id: 0,
                    timeout_ms: 10_000,
                },
            ),
            reply,
            deadline: Instant::now() + Duration::from_secs(12),
        });
        check(
            waiter.try_recv().is_err(),
            "completion waiter did not remain pending before detach",
        )?;
        let session_counter = self.next_session;
        let program = std::mem::replace(
            &mut self.config.shell.program,
            "/mechanic-native-smoke-no-such-shell".into(),
        );
        let destination = self.detach_pane(events, source, pane_id, None);
        self.config.shell.program = program;
        let destination = destination.ok_or("live pane detachment failed")?;
        eprintln!("live move smoke: pane detached");
        check(
            destination != source && self.next_session == session_counter,
            "detachment created a replacement PTY identity",
        )?;
        check(self.windows[&source].tree.len() == 1, "detachment did not collapse source layout")?;
        {
            let state = self.windows.get_mut(&destination).ok_or("detached window absent")?;
            check(state.pane.session == session, "detachment changed control session identity")?;
            check(
                state.pane.terminal.selection_text() == selection,
                "detachment lost selected text",
            )?;
            check(state.pane.search.active, "detachment lost Find state")?;
            state.pane.search.navigate(&state.pane.terminal, false);
            check(state.pane.search.current().is_some(), "detachment lost Find query")?;
            check(
                state.pane.preedit.is_none()
                    && state.pane.search_panel.is_none()
                    && !state.pane.mouse_pressed,
                "detachment retained window-bound input state",
            )?;
        }
        check(
            self.session_routes[&session].resolve(source, pane_id) == Some((destination, 1)),
            "original PTY reader wake did not retarget",
        )?;
        // Exercise a wake queued with the old IDs; then leave the queue empty so
        // the delayed command below must be parsed through an actual reader wake.
        while self.pending_parsers.pop().is_some() {}
        self.user_event(events, UserEvent::PtyOutput(source, pane_id, session));
        check(
            self.pending_parsers.pop() == Some((destination, 1, session)),
            "queued old-address PTY wake did not follow live pane",
        )?;
        self.windows.get_mut(&destination).unwrap().pane.terminal.write_to_pty(
            b"sleep 0.1; printf 'LIVE_MOVE_%s\\n' \"$mechanic_live_token\"; printf '\\033]133;D;23\\007'\n"
        ).map_err(|error| error.to_string())?;
        // Remove the original window entirely. Future wakes retain its old IDs,
        // and must still reach the live terminal in the destination window.
        self.close_window(source, events);
        eprintln!("live move smoke: original window closed");
        check(!self.windows.contains_key(&source), "original native window remained")?;
        Ok((
            LivePaneMoveFixture {
                socket,
                selector,
                waiter,
                expected_output: format!("LIVE_MOVE_{token}"),
                expected_window: format!("{destination:?}"),
            },
            directory,
        ))
    }

    fn live_move_wait_text(
        &mut self,
        events: &ActiveEventLoop,
        window: WindowId,
        pane: PaneId,
        expected: &str,
    ) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = self.windows.get_mut(&window).ok_or("live move source closed")?;
            state.load_pane(pane);
            let session = state.pane.session;
            let text: String =
                state.pane.terminal.grid().display_iter().map(|cell| cell.cell.c).collect();
            if text.contains(expected) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("live shell did not produce {expected:?}"));
            }
            self.pending_parsers.enqueue((window, pane, session));
            self.pump_parser(events);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl LivePaneMoveFixture {
    pub(crate) fn exercise(self) -> Result<(), String> {
        eprintln!("live move smoke: waiting for original reader completion");
        let completion = self.waiter.recv_timeout(Duration::from_secs(12)).map_err(|error| {
            format!("original reader wake/pre-detach completion waiter failed: {error}")
        })?;
        check(
            matches!(&completion.result, ResponseResult::Completed { completion }
            if completion.session == self.selector && completion.exit_status == Some(23)),
            "pre-detach completion waiter changed session or status",
        )?;
        let call = |operation| {
            crate::control::request(
                &self.socket,
                &Request::new(Some(self.selector.instance_id.clone()), operation),
            )
            .map_err(|error| error.to_string())
        };
        let output = call(Operation::ReadOutput {
            session: self.selector.clone(),
            max_lines: 500,
            max_bytes: 128 * 1024,
        })?;
        let ResponseResult::Output { text, session, .. } = output.result else {
            return Err("detached control handle could not read output".into());
        };
        check(
            session == self.selector && text.contains(&self.expected_output),
            "live shell variable/output did not survive detachment",
        )?;
        check(
            text.contains("LIVE_HISTORY_SENTINEL") && text.contains("LIVE_SELECTION_TOKEN"),
            "detachment discarded terminal history",
        )?;
        let list = call(Operation::ListPanes)?;
        let ResponseResult::Panes { panes } = list.result else {
            return Err("detached pane list failed".into());
        };
        check(
            panes.iter().any(|pane| {
                pane.session == self.selector && pane.window_id == self.expected_window
            }),
            "stable control handle did not identify detached window",
        )?;
        Ok(())
    }
}
