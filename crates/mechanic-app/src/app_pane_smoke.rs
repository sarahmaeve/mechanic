use super::*;
use std::path::Path;
use winit::event::{DeviceId, MouseScrollDelta, TouchPhase};

fn check(condition: bool, message: &str) -> Result<(), String> {
    condition.then_some(()).ok_or_else(|| message.to_owned())
}

fn visible_text(terminal: &Terminal) -> String {
    terminal.grid().display_iter().map(|cell| cell.cell.c).collect()
}

impl App {
    pub(crate) fn smoke_hide_windows(&mut self) {
        self.hidden_windows = true;
    }

    pub(crate) fn smoke_window_count(&self) -> usize {
        self.windows.len()
    }

    fn smoke_pointer(&mut self, events: &ActiveEventLoop, window: WindowId, x: f64, y: f64) {
        self.window_event(
            events,
            window,
            WindowEvent::CursorMoved {
                device_id: DeviceId::dummy(),
                position: PhysicalPosition::new(x, y),
            },
        );
    }

    fn smoke_button(&mut self, events: &ActiveEventLoop, window: WindowId, pressed: bool) {
        self.window_event(
            events,
            window,
            WindowEvent::MouseInput {
                device_id: DeviceId::dummy(),
                button: MouseButton::Left,
                state: if pressed { ElementState::Pressed } else { ElementState::Released },
            },
        );
    }

    fn smoke_wait_text(
        &mut self,
        events: &ActiveEventLoop,
        window: WindowId,
        pane: PaneId,
        expected: &str,
    ) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = self.windows.get_mut(&window).ok_or("smoke window closed")?;
            check(state.load_pane(pane), "smoke pane disappeared")?;
            let session = state.pane.session;
            let found = visible_text(&state.pane.terminal).contains(expected);
            state.load_pane(state.tree.active());
            if found {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("PTY did not produce {expected:?}"));
            }
            self.pending_parsers.enqueue((window, pane, session));
            self.pump_parser(events);
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    pub(crate) fn smoke_exercise_panes(
        &mut self,
        events: &ActiveEventLoop,
        directory: &Path,
    ) -> Result<(), String> {
        check(self.windows.len() == 1, "expected one initial window")?;
        let window = *self.windows.keys().next().unwrap();
        let first = self.windows[&window].tree.active();
        {
            let state = self.windows.get_mut(&window).unwrap();
            state.pane.terminal.inject_local(
                format!("\x1b]7;file://localhost{}\x07", directory.display()).as_bytes(),
            );
            // inject_local bypasses the app's normal parser-completion hook.
            app_session::refresh_directory(&mut state.pane);
        }
        let second = self.split_pane(window, Axis::Vertical).ok_or("vertical split failed")?;
        let second_session = self.windows[&window].pane.session;
        {
            let state = self.windows.get_mut(&window).unwrap();
            state
                .pane
                .terminal
                .write_to_pty(b"printf '\\nINHERITED:%s\\n' \"$PWD\"\n")
                .map_err(|e| e.to_string())?;
        }
        self.smoke_wait_text(
            events,
            window,
            second,
            &format!("INHERITED:{}", directory.display()),
        )?;
        let third = self.split_pane(window, Axis::Horizontal).ok_or("horizontal split failed")?;
        check(self.windows[&window].tree.len() == 3, "split tree lost a pane")?;

        self.window_event(
            events,
            window,
            WindowEvent::Ime(Ime::Preedit("日本語".into(), Some((0, 9)))),
        );
        {
            let state = self.windows.get_mut(&window).unwrap();
            check(
                state.loaded_pane == third && state.pane.preedit.is_some(),
                "IME preedit did not reach active pane",
            )?;
            state.focus_pane(first);
            state.load_pane(third);
            check(state.pane.preedit.is_none(), "focus change retained old pane composition")?;
            state.load_pane(first);
        }

        self.window_event(events, window, WindowEvent::Ime(Ime::Commit("STALE_COMMIT".into())));
        check(!self.windows[&window].cancelled_ime_commit, "stale IME commit was not consumed")?;
        self.window_event(events, window, WindowEvent::Ime(Ime::Preedit("ö".into(), Some((0, 2)))));
        self.windows.get_mut(&window).unwrap().focus_pane(third);
        self.window_event(events, window, WindowEvent::Ime(Ime::Enabled));
        check(
            !self.windows[&window].cancelled_ime_commit,
            "fresh IME session retained stale-commit suppression",
        )?;
        self.window_event(
            events,
            window,
            WindowEvent::Ime(Ime::Commit("printf '\\nIME_%s\\n' READY\n".into())),
        );
        self.smoke_wait_text(events, window, third, "IME_READY")?;
        self.window_event(
            events,
            window,
            WindowEvent::Ime(Ime::Preedit("日本語".into(), Some((0, 9)))),
        );
        self.window_event(events, window, WindowEvent::Focused(false));
        check(
            self.windows[&window].cancelled_ime_commit
                && self.windows[&window].pane.preedit.is_none(),
            "focus loss did not cancel native composition",
        )?;
        self.window_event(events, window, WindowEvent::Ime(Ime::Disabled));
        self.window_event(events, window, WindowEvent::Focused(true));

        for (pane, label) in [(first, "LEFT"), (second, "RIGHT"), (third, "BOTTOM")] {
            let state = self.windows.get_mut(&window).unwrap();
            state.load_pane(pane);
            let mut text = String::from("\x1b[2J\x1b[H");
            for _ in 0..100 {
                text.push_str(&format!("{label} Straße 日本語 مرحبًا\r\n"));
            }
            state.pane.terminal.inject_local(text.as_bytes());
            state.pane.search.active = true;
            state.pane.search.set_query(label.to_owned(), &state.pane.terminal);
            state.mark_content_dirty();
        }
        {
            let state = self.windows.get_mut(&window).unwrap();
            state.focus_pane(first);
            state.load_pane(first);
            state
                .pane
                .terminal
                .start_selection(GridPoint::new(GridLine(0), GridColumn(0)), GridSide::Left);
            state
                .pane
                .terminal
                .update_selection(GridPoint::new(GridLine(0), GridColumn(3)), GridSide::Right);
            check(
                state.pane.terminal.selection_text().as_deref() == Some("LEFT"),
                "first pane selection failed",
            )?;
        }
        self.user_event(
            events,
            UserEvent::Search(
                window,
                second,
                second_session,
                crate::search_platform::SearchAction::Query("STRASSE".into()),
            ),
        );
        {
            let state = self.windows.get_mut(&window).unwrap();
            check(state.tree.active() == first, "background Find stole pane focus")?;
            state.load_pane(first);
            check(
                state.pane.terminal.selection_text().as_deref() == Some("LEFT"),
                "Find changed another pane's selection",
            )?;
            let hit = state.pane.search.current().ok_or("first pane search disappeared")?;
            check(hit.start.column.0 == 0, "Find changed another pane's query")?;
        }

        let right = self.windows[&window].layout.pane(second).ok_or("right rectangle missing")?;
        self.smoke_pointer(events, window, f64::from(right.x) + 10.0, f64::from(right.y) + 10.0);
        self.window_event(
            events,
            window,
            WindowEvent::MouseWheel {
                device_id: DeviceId::dummy(),
                delta: MouseScrollDelta::LineDelta(0.0, 3.0),
                phase: TouchPhase::Moved,
            },
        );
        {
            let state = self.windows.get_mut(&window).unwrap();
            check(
                state.tree.active() == first && state.loaded_pane == first,
                "wheel changed keyboard focus or leaked routing",
            )?;
            check(
                state.pane.terminal.grid().display_offset() == 0,
                "wheel scrolled the wrong pane",
            )?;
            state.load_pane(second);
            check(
                state.pane.terminal.grid().display_offset() > 0,
                "wheel did not scroll the hovered pane",
            )?;
            state.load_pane(first);
        }
        self.smoke_button(events, window, true);
        self.smoke_button(events, window, false);
        check(self.windows[&window].tree.active() == second, "click did not focus hovered pane")?;

        self.smoke_button(events, window, true);
        self.windows.get_mut(&window).unwrap().focus_pane(first);
        self.window_event(events, window, WindowEvent::ModifiersChanged(Default::default()));
        self.smoke_button(events, window, false);
        {
            let state = self.windows.get_mut(&window).unwrap();
            check(state.tree.active() == first, "drag release stole keyboard focus")?;
            state.load_pane(second);
            check(
                !state.pane.mouse_pressed
                    && state.pane.held_buttons.report_button(false, true).is_none(),
                "focus change left mouse buttons held in old pane",
            )?;
            state.load_pane(first);
        }

        let divider = self.windows[&window].layout.dividers[0];
        let old_first = self.windows[&window].layout.pane(first).unwrap();
        let x = f64::from(divider.rect.x) + f64::from(divider.rect.width) / 2.0;
        let y = f64::from(divider.rect.y) + 10.0;
        self.smoke_pointer(events, window, x, y);
        self.smoke_button(events, window, true);
        self.smoke_pointer(events, window, x + 40.0, y);
        self.smoke_button(events, window, false);
        check(
            self.windows[&window].layout.pane(first).unwrap().width != old_first.width,
            "divider drag did not resize panes",
        )?;

        {
            let state = self.windows.get_mut(&window).unwrap();
            App::apply_font_size(state, 17.0);
            for item in state.layout.panes.clone() {
                state.load_pane(item.id);
                let expected = App::terminal_size_from_metrics(
                    item.rect.width,
                    item.rect.height,
                    &state.cell_metrics,
                );
                check(
                    state.pane.terminal.size() == expected,
                    "pane PTY size differs from viewport after font resize",
                )?;
            }
            state.load_pane(state.tree.active());
        }
        self.window_event(events, window, WindowEvent::RedrawRequested);
        {
            let state = self.windows.get_mut(&window).unwrap();
            for pane in state.tree.pane_ids() {
                state.load_pane(pane);
                check(
                    state.pane.cached_grid.is_some(),
                    "pane was omitted from render preparation",
                )?;
            }
            state.load_pane(state.tree.active());
        }

        // The same constructor used by Cmd+N receives a local inherited path.
        let extra =
            self.spawn_window(events, Some(directory)).ok_or("inherited new window failed")?;
        let extra_pane = self.windows[&extra].tree.active();
        self.windows
            .get_mut(&extra)
            .unwrap()
            .pane
            .terminal
            .write_to_pty(b"printf '\\nWINDOW_CWD:%s\\n' \"$PWD\"\n")
            .map_err(|e| e.to_string())?;
        self.smoke_wait_text(
            events,
            extra,
            extra_pane,
            &format!("WINDOW_CWD:{}", directory.display()),
        )?;
        self.close_window(extra, events);
        let old_session = {
            let state = self.windows.get_mut(&window).unwrap();
            state.focus_pane(third);
            state.pane.session
        };
        let new_session = self.allocate_session().ok_or("session IDs exhausted")?;
        let waker = make_waker_for(&self.proxy, window, third, new_session);
        {
            let state = self.windows.get_mut(&window).unwrap();
            state.pane.terminal.inject_local(
                format!("\x1b]7;file://localhost{}\x07", directory.display()).as_bytes(),
            );
            state.pane.exit_status = Some(None);
            respawn_shell(state, &self.config, window, waker, new_session);
            check(
                state.pane.session == new_session && state.pane.exit_status.is_none(),
                "pane restart did not advance session",
            )?;
            state
                .pane
                .terminal
                .write_to_pty(b"printf '\\nRESTART:%s\\n' \"$PWD\"\n")
                .map_err(|e| e.to_string())?;
        }
        self.smoke_wait_text(events, window, third, &format!("RESTART:{}", directory.display()))?;
        {
            let state = self.windows.get_mut(&window).unwrap();
            state.pane.search.active = true;
            state.pane.search.set_query("RESTART".into(), &state.pane.terminal);
        }
        self.user_event(
            events,
            UserEvent::Search(
                window,
                third,
                old_session,
                crate::search_platform::SearchAction::Query("STALE_QUERY".into()),
            ),
        );
        check(
            self.windows[&window].pane.search.current().is_some(),
            "stale search callback changed restarted pane",
        )?;
        self.user_event(events, UserEvent::PtyOutput(window, third, old_session));
        self.close_pane(window, third, events);
        self.user_event(events, UserEvent::PtyOutput(window, third, old_session));
        check(self.windows[&window].tree.len() == 2, "closing pane did not collapse tree")?;
        self.close_pane(window, second, events);
        {
            let state = self.windows.get(&window).unwrap();
            check(
                state.tree.len() == 1 && state.tree.active() == first,
                "surviving pane identity changed",
            )?;
            check(state.layout.dividers.is_empty(), "closed split retained a divider")?;
            let size = state.window.inner_size();
            check(
                state.layout.pane(first).unwrap()
                    == Rect { x: 0, y: 0, width: size.width, height: size.height },
                "last pane did not fill window",
            )?;
        }
        Ok(())
    }
}
