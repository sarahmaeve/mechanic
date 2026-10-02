//! Explicit native smoke helpers; normal application builds omit this module.
use super::*;
use crate::control::{ErrorCode, InputMode, Operation, Request, ResponseResult, SessionSelector};
use std::path::{Path, PathBuf};

pub(crate) struct SessionControlFixture {
    socket: PathBuf,
    control_directory: PathBuf,
    target: SessionSelector,
    stale: SessionSelector,
    active: Vec<SessionSelector>,
    directory: PathBuf,
}

fn check(condition: bool, message: &str) -> Result<(), String> {
    condition.then_some(()).ok_or_else(|| message.to_owned())
}

impl App {
    /// Save a real two-window workspace, drop every old PTY, and restore new PTYs.
    pub(crate) fn smoke_restore_session_control(
        &mut self,
        events: &ActiveEventLoop,
        directory: &Path,
    ) -> Result<SessionControlFixture, String> {
        check(self.windows.len() == 1, "expected one initial hidden window")?;
        let state_directory = directory.join("state");
        let control_directory = directory.join("ctl");
        let shell_directory = directory.join("cwd-Grüße-日本語");
        std::fs::create_dir(&shell_directory).map_err(|err| err.to_string())?;
        let shell_directory = shell_directory.canonicalize().map_err(|err| err.to_string())?;
        self.configure_services(
            false,
            Some(state_directory.clone()),
            Some(control_directory.clone()),
        );
        let first_window = *self.windows.keys().next().unwrap();
        let target_pane = self.windows[&first_window].tree.active();
        let target_session = self.windows[&first_window].pane.session;
        let stale = SessionSelector {
            instance_id: self
                .control_service
                .server
                .as_ref()
                .ok_or("control server failed")?
                .instance_id()
                .to_owned(),
            session_id: target_session,
        };
        self.split_pane(first_window, Axis::Vertical).ok_or("first split failed")?;
        let active_pane =
            self.split_pane(first_window, Axis::Horizontal).ok_or("second split failed")?;
        {
            let state = self.windows.get_mut(&first_window).unwrap();
            let divider = state.layout.dividers[0];
            let size = state.window.inner_size();
            check(
                state.tree.drag_divider(
                    divider.id,
                    f64::from(size.width) * 0.37,
                    f64::from(size.height) * 0.37,
                    &state.layout,
                ),
                "divider adjustment failed",
            )?;
            state.resize_panes();
            App::apply_font_size(state, 17.0);
            for pane in state.tree.pane_ids() {
                state.load_pane(pane);
                state.pane.directory = Some(shell_directory.clone());
                state.pane.terminal.inject_local(b"OLD_CONTENT_MUST_NOT_RESTORE");
            }
            state.focus_pane(active_pane);
            state.load_pane(active_pane);
        }
        let second_window =
            self.spawn_window(events, Some(&shell_directory)).ok_or("second window failed")?;
        App::apply_font_size(self.windows.get_mut(&second_window).unwrap(), 15.0);
        self.note_session_change();
        self.flush_session();
        let expected = self.workspace_snapshot();
        check(state_directory.join("session.json").is_file(), "session was not saved")?;

        let mut restored =
            App::new(self.config.clone(), self.proxy.clone(), self.animations, self.mouse_tracking);
        restored.smoke_hide_windows();
        // Drop releases the single-writer lock before the next instance opens it.
        let old = std::mem::replace(self, restored);
        drop(old);
        self.configure_services(true, Some(state_directory), Some(control_directory.clone()));
        self.resumed(events);
        check(self.windows.len() == 2, "restore did not recreate both windows")?;
        let mut expected_windows = expected.windows;
        let mut actual_windows = self.workspace_snapshot().windows;
        expected_windows.sort_by(|left, right| left.font_size.total_cmp(&right.font_size));
        actual_windows.sort_by(|left, right| left.font_size.total_cmp(&right.font_size));
        for (expected, actual) in expected_windows.iter().zip(&actual_windows) {
            check(
                expected.panes == actual.panes,
                "restore changed split axes, ratio, or active pane",
            )?;
            check(expected.font_size == actual.font_size, "restore changed font size")?;
            check(expected.directories == actual.directories, "restore changed pane directories")?;
            check(
                expected
                    .logical_size
                    .iter()
                    .zip(actual.logical_size)
                    .all(|(left, right)| (left - right).abs() <= 2.0),
                "restore changed logical window geometry",
            )?;
        }
        let server =
            self.control_service.server.as_ref().ok_or("restored control server failed")?;
        let instance = server.instance_id().to_owned();
        check(instance != stale.instance_id, "restored instance reused its control nonce")?;
        let socket = server.socket_path().to_owned();
        let mut target = None;
        let mut active = Vec::new();
        for state in self.windows.values() {
            for pane in state.tree.pane_ids() {
                let terminal = state.pane_state(pane).ok_or("restored pane absent")?;
                check(
                    !terminal
                        .terminal
                        .grid()
                        .display_iter()
                        .map(|cell| cell.cell.c)
                        .collect::<String>()
                        .contains("OLD_CONTENT_MUST_NOT_RESTORE"),
                    "restored terminal captured old content",
                )?;
                let selector =
                    SessionSelector { instance_id: instance.clone(), session_id: terminal.session };
                if state.tree.active() == pane {
                    active.push(selector.clone());
                }
                if state.current_font_size == 17.0 && pane == target_pane {
                    target = Some(selector);
                }
            }
        }
        let target = target.ok_or("target pane absent")?;
        let (target_window, target_pane) = self
            .windows
            .iter()
            .find_map(|(window, state)| {
                state
                    .tree
                    .pane_ids()
                    .into_iter()
                    .find(|pane| {
                        state
                            .pane_state(*pane)
                            .is_some_and(|pane| pane.session == target.session_id)
                    })
                    .map(|pane| (*window, pane))
            })
            .ok_or("silent-input target absent")?;
        {
            let state = self.windows.get_mut(&target_window).unwrap();
            state.load_pane(target_pane);
            let history = "silent-input history\r\n".repeat(state.pane.terminal.screen_lines() * 2);
            state.pane.terminal.inject_local(history.as_bytes());
            state.pane.terminal.scroll_up(1);
            check(
                state.pane.terminal.grid().display_offset() > 0,
                "silent-input viewport did not scroll",
            )?;
            state.content_dirty = false;
            state.pane.content_dirty = false;
        }
        let (reply, response) = std::sync::mpsc::sync_channel(1);
        self.dispatch_control(
            events,
            crate::control::ControlEvent {
                request: Request::new(
                    Some(instance.clone()),
                    Operation::SendInput {
                        session: target.clone(),
                        text: String::new(),
                        mode: InputMode::Raw,
                        enter: false,
                    },
                ),
                reply,
                deadline: Instant::now() + Duration::from_secs(5),
            },
        );
        check(
            matches!(
                response.try_recv().map_err(|error| error.to_string())?.result,
                ResponseResult::InputSent { .. }
            ),
            "silent-input control request failed",
        )?;
        let state = self.windows.get(&target_window).unwrap();
        let pane = state.pane_state(target_pane).ok_or("silent-input target disappeared")?;
        check(
            pane.terminal.grid().display_offset() == 0,
            "control input left a scrolled viewport",
        )?;
        check(
            state.content_dirty && pane.content_dirty,
            "silent control input did not request viewport redraw",
        )?;
        {
            let state = self.windows.get_mut(&target_window).unwrap();
            state.load_pane(target_pane);
            state.pane.content_dirty = false;
            state.content_dirty = false;
            state.load_pane(state.tree.active());
        }
        let appearance = crate::session::PaneAppearance {
            text_color: Some("#123ABC".into()),
            ..Default::default()
        };
        self.set_pane_appearance(target_window, target_pane, appearance)?;
        let state = &self.windows[&target_window];
        check(
            state.tree.active() != target_pane
                && state.pane_state(target_pane).is_some_and(|pane| pane.content_dirty),
            "background appearance update did not invalidate target grid",
        )?;
        self.set_pane_appearance(target_window, target_pane, Default::default())?;

        Ok(SessionControlFixture {
            socket,
            control_directory,
            target,
            stale,
            active,
            directory: shell_directory,
        })
    }
}

impl SessionControlFixture {
    /// Runs on a worker while winit forwards the real control requests to App.
    pub(crate) fn exercise(self) -> Result<(), String> {
        use crate::control;
        let call = |operation| {
            control::request(
                &self.socket,
                &Request::new(Some(self.target.instance_id.clone()), operation),
            )
            .map_err(|err| err.to_string())
        };
        let endpoints = control::discover_endpoints_in(&self.control_directory)
            .map_err(|err| err.to_string())?;
        check(
            endpoints.len() == 1 && endpoints[0].socket_path == self.socket,
            "instance discovery failed",
        )?;
        let list = || -> Result<Vec<control::PaneInfo>, String> {
            match call(Operation::ListPanes)?.result {
                ResponseResult::Panes { panes } => Ok(panes),
                result => Err(format!("unexpected list result: {result:?}")),
            }
        };
        let panes = list()?;
        check(panes.len() == 4, "list did not report all restored panes")?;
        check(
            panes.iter().any(|pane| {
                pane.session == self.target && pane.cwd.as_deref() == self.directory.to_str()
            }),
            "list did not expose restored cwd",
        )?;
        check(
            panes.iter().all(|pane| pane.latest_command_id == 0),
            "fresh PTYs inherited old command IDs",
        )?;
        let output = || -> Result<String, String> {
            match call(Operation::ReadOutput {
                session: self.target.clone(),
                max_lines: 200,
                max_bytes: 8192,
            })?
            .result
            {
                ResponseResult::Output { text, .. } => Ok(text),
                result => Err(format!("unexpected read result: {result:?}")),
            }
        };
        let send = |text: String, mode, enter| -> Result<(), String> {
            match call(Operation::SendInput { session: self.target.clone(), text, mode, enter })?
                .result
            {
                ResponseResult::InputSent { .. } => Ok(()),
                result => Err(format!("unexpected send result: {result:?}")),
            }
        };
        let wait = |after, timeout_ms| {
            call(Operation::Wait {
                session: self.target.clone(),
                after_command_id: after,
                timeout_ms,
            })
        };
        let expected = format!("RESTORE_CWD:{}", self.directory.display());
        send(
            "printf '\\033]133;C\\007RESTORE_CWD:%s\\nGrüße 日本語\\n\\033]133;D;0\\007' \"$PWD\""
                .into(),
            InputMode::Paste,
            false,
        )?;
        std::thread::sleep(Duration::from_millis(40));
        check(!output()?.contains(&expected), "paste executed without explicit Enter")?;
        send(String::new(), InputMode::Paste, true)?;
        let first = match wait(0, 5000)?.result {
            ResponseResult::Completed { completion } => completion,
            result => return Err(format!("command did not complete: {result:?}")),
        };
        check(first.command_id == 1 && first.exit_status == Some(0), "wrong first completion")?;
        let text = output()?;
        if !text.contains(&expected) || !text.contains("Grüße 日本語") {
            return Err(format!(
                "restored PTY cwd/Unicode mismatch: expected {expected:?}, read {text:?}"
            ));
        }
        match wait(0, 100)?.result {
            ResponseResult::Completed { completion } => check(
                completion.command_id == first.command_id,
                "fast completion race lost retained event",
            )?,
            result => return Err(format!("retained completion missing: {result:?}")),
        }
        check(
            matches!(wait(first.command_id, 25)?.result, ResponseResult::Timeout { .. }),
            "wait returned idle as completion",
        )?;
        send(
            "printf '\\033]133;C\\007'; sleep 0.1; printf 'RAW_%s\\n\\033]133;D;7\\007' READY\n"
                .into(),
            InputMode::Raw,
            false,
        )?;
        match wait(first.command_id, 5000)?.result {
            ResponseResult::Completed { completion } => check(
                completion.command_id == 2 && completion.exit_status == Some(7),
                "queued wait completion incorrect",
            )?,
            result => return Err(format!("queued wait failed: {result:?}")),
        }
        check(output()?.contains("RAW_READY"), "raw newline did not reach PTY")?;
        let stale_response = control::request(
            &self.socket,
            &Request::new(
                Some(self.stale.instance_id.clone()),
                Operation::SendInput {
                    session: self.stale.clone(),
                    text: "STALE_MUST_NOT_WRITE".into(),
                    mode: InputMode::Raw,
                    enter: false,
                },
            ),
        )
        .map_err(|err| err.to_string())?;
        check(
            matches!(
                stale_response.result,
                ResponseResult::Error { code: ErrorCode::StaleInstance, .. }
            ),
            "stale instance handle was accepted",
        )?;
        let absent =
            SessionSelector { instance_id: self.target.instance_id.clone(), session_id: u64::MAX };
        check(
            matches!(
                call(Operation::ReadOutput { session: absent, max_lines: 1, max_bytes: 64 })?
                    .result,
                ResponseResult::Error { code: ErrorCode::UnknownSession, .. }
            ),
            "unknown terminal session was accepted",
        )?;
        check(!output()?.contains("STALE_MUST_NOT_WRITE"), "stale input reached restored shell")?;
        let active: Vec<_> =
            list()?.into_iter().filter(|pane| pane.active).map(|pane| pane.session).collect();
        check(
            active.len() == self.active.len()
                && active.iter().all(|pane| self.active.contains(pane)),
            "control operation stole pane focus",
        )?;
        self.exercise_workspace_control()?;
        Ok(())
    }

    fn exercise_workspace_control(&self) -> Result<(), String> {
        use crate::control::{self, AppearancePatch, MoveEdge, PaneInfo};
        let call = |operation| {
            control::request(
                &self.socket,
                &Request::new(Some(self.target.instance_id.clone()), operation),
            )
            .map_err(|error| error.to_string())
        };
        let pane = |operation| -> Result<PaneInfo, String> {
            match call(operation)?.result {
                ResponseResult::Pane { pane } => Ok(pane),
                result => Err(format!("unexpected pane mutation result: {result:?}")),
            }
        };
        let list = || -> Result<Vec<PaneInfo>, String> {
            match call(Operation::ListPanes)?.result {
                ResponseResult::Panes { panes } => Ok(panes),
                result => Err(format!("unexpected workspace list: {result:?}")),
            }
        };
        let original: Vec<_> = list()?.into_iter().map(|pane| pane.session).collect();
        check(
            matches!(
                call(Operation::CreatePane {
                    session: None,
                    directory: Some(self.directory.join("missing")),
                })?
                .result,
                ResponseResult::Error { code: ErrorCode::InvalidRequest, .. }
            ),
            "nonexistent explicit directory was accepted",
        )?;
        check(list()?.len() == original.len(), "invalid creation changed workspace")?;
        let created =
            pane(Operation::CreatePane { session: Some(self.target.clone()), directory: None })?;
        check(
            created.cwd.as_deref() == self.directory.to_str(),
            "creation did not inherit directory",
        )?;
        check(!original.contains(&created.session), "creation reused a session handle")?;
        let split = pane(Operation::SplitPane {
            session: created.session.clone(),
            axis: Axis::Vertical,
            directory: Some(self.directory.clone()),
        })?;
        check(
            split.session != created.session && split.window_id == created.window_id,
            "split did not create a new session in the selected window",
        )?;
        check(
            pane(Operation::FocusPane { session: created.session.clone() })?.active,
            "focus did not select the target pane",
        )?;
        let styled = pane(Operation::SetPane {
            session: split.session.clone(),
            appearance: AppearancePatch {
                title: Some(Some("Control 日本語".into())),
                text_color: Some(Some("#aBcDeF".into())),
                outline_color: Some(Some("#13579b".into())),
            },
        })?;
        check(
            styled.title == "Control 日本語"
                && styled.appearance.text_color.as_deref() == Some("#ABCDEF")
                && styled.appearance.outline_color.as_deref() == Some("#13579B"),
            "appearance update did not normalize or expose values",
        )?;
        let focused = pane(Operation::FocusPane { session: created.session.clone() })?;
        check(focused.active, "focus did not select the target pane")?;
        check(
            pane(Operation::ZoomPane { session: split.session.clone() })?.zoomed,
            "zoom did not select and enlarge target",
        )?;
        check(
            !pane(Operation::ZoomPane { session: split.session.clone() })?.zoomed,
            "second zoom did not restore layout",
        )?;
        match call(Operation::SendInput {
            session: split.session.clone(),
            text: "printf '\\033]133;C\\007CONTROL_MOVE_%s\\n\\033]133;D;0\\007' PERSISTED".into(),
            mode: InputMode::Paste,
            enter: true,
        })?
        .result
        {
            ResponseResult::InputSent { .. } => {}
            result => return Err(format!("move sentinel input failed: {result:?}")),
        }
        check(
            matches!(
                call(Operation::Wait {
                    session: split.session.clone(),
                    after_command_id: 0,
                    timeout_ms: 5000
                })?
                .result,
                ResponseResult::Completed { .. }
            ),
            "move sentinel did not complete",
        )?;
        let moved = pane(Operation::MovePane {
            session: split.session.clone(),
            target: Some(self.target.clone()),
            edge: Some(MoveEdge::Right),
            new_window: false,
        })?;
        check(
            moved.session == split.session
                && moved.window_id != split.window_id
                && moved.appearance == styled.appearance,
            "dock lost session identity or appearance",
        )?;
        let detached = pane(Operation::MovePane {
            session: split.session.clone(),
            target: None,
            edge: None,
            new_window: true,
        })?;
        check(
            detached.session == split.session && detached.window_id != moved.window_id,
            "detach did not preserve live session",
        )?;
        match call(Operation::ReadOutput {
            session: split.session.clone(),
            max_lines: 200,
            max_bytes: 8192,
        })?
        .result
        {
            ResponseResult::Output { text, .. } => check(
                text.contains("CONTROL_MOVE_PERSISTED"),
                "live pane move lost terminal content",
            )?,
            result => return Err(format!("moved output unavailable: {result:?}")),
        }
        let name = "native control workspace".to_owned();
        check(
            matches!(
                call(Operation::SaveLoadout { name: name.clone(), include_directories: true })?
                    .result,
                ResponseResult::LoadoutSaved { .. }
            ),
            "loadout save failed",
        )?;
        match call(Operation::ListLoadouts)?.result {
            ResponseResult::Loadouts { loadouts } => check(
                loadouts.iter().any(|loadout| {
                    loadout.name == name
                        && loadout.include_directories
                        && loadout.panes == original.len() + 2
                }),
                "saved loadout summary is incorrect",
            )?,
            result => return Err(format!("loadout list failed: {result:?}")),
        }
        let opened =
            match call(Operation::OpenLoadout { name: name.clone(), restore_directories: true })?
                .result
            {
                ResponseResult::LoadoutOpened { panes, .. } => panes,
                result => return Err(format!("loadout open failed: {result:?}")),
            };
        check(
            opened.len() == original.len() + 2
                && opened.iter().all(|pane| {
                    !original.contains(&pane.session)
                        && pane.session != created.session
                        && pane.session != split.session
                }),
            "opened loadout reused an old session",
        )?;
        check(
            opened.iter().any(|pane| {
                pane.appearance == styled.appearance
                    && pane.cwd.as_deref() == self.directory.to_str()
            }),
            "opened loadout lost appearance or directory",
        )?;
        check(
            matches!(
                call(Operation::SaveLoadout { name: name.clone(), include_directories: false })?
                    .result,
                ResponseResult::LoadoutSaved { .. }
            ),
            "directory-free save failed",
        )?;
        match call(Operation::ListLoadouts)?.result {
            ResponseResult::Loadouts { loadouts } => check(
                loadouts.iter().any(|loadout| loadout.name == name && !loadout.include_directories),
                "directory-free save retained directory flag",
            )?,
            result => return Err(format!("loadout list after replacement failed: {result:?}")),
        }
        let cleared = pane(Operation::SetPane {
            session: split.session.clone(),
            appearance: AppearancePatch {
                title: Some(None),
                text_color: None,
                outline_color: Some(None),
            },
        })?;
        check(
            cleared.appearance.title.is_none()
                && cleared.appearance.outline_color.is_none()
                && cleared.appearance.text_color == styled.appearance.text_color,
            "appearance clear changed an omitted field",
        )?;
        check(
            matches!(
                call(Operation::DeleteLoadout { name: name.clone() })?.result,
                ResponseResult::LoadoutDeleted { .. }
            ),
            "loadout delete failed",
        )?;
        match call(Operation::ListLoadouts)?.result {
            ResponseResult::Loadouts { loadouts } => check(
                !loadouts.iter().any(|loadout| loadout.name == name),
                "deleted loadout remained listed",
            )?,
            result => return Err(format!("loadout list after delete failed: {result:?}")),
        }
        for session in opened
            .into_iter()
            .map(|pane| pane.session)
            .chain([created.session, split.session.clone()])
        {
            check(
                matches!(call(Operation::ClosePane { session: session.clone() })?.result, ResponseResult::PaneClosed { session: closed } if closed == session),
                "pane close failed",
            )?;
        }
        check(
            list()?.len() == original.len(),
            "workspace cleanup did not restore original pane count",
        )?;
        check(
            matches!(
                call(Operation::ReadOutput {
                    session: split.session,
                    max_lines: 1,
                    max_bytes: 64
                })?
                .result,
                ResponseResult::Error { code: ErrorCode::UnknownSession, .. }
            ),
            "closed handle still addressed a terminal",
        )?;
        Ok(())
    }
}
