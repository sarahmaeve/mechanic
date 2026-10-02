//! Opt-in native workspace integration check.
use super::*;

fn check(value: bool, message: &str) -> Result<(), String> {
    value.then_some(()).ok_or_else(|| message.to_owned())
}

impl App {
    pub(crate) fn smoke_workspace_features(
        &mut self,
        events: &ActiveEventLoop,
        directory: &std::path::Path,
    ) -> Result<(), String> {
        self.configure_services(false, Some(directory.join("workspace")), None);
        let launch = directory.join("Grüße-日本語-العربية");
        std::fs::create_dir(&launch).map_err(|e| e.to_string())?;
        let launch = launch.canonicalize().map_err(|e| e.to_string())?;
        let window = self.spawn_window(events, Some(&launch)).ok_or("create window")?;
        let pane = self.windows[&window].tree.active();
        let session = self.windows[&window].pane.session;
        self.windows
            .get_mut(&window)
            .unwrap()
            .pane
            .terminal
            .write_to_pty(b"mechanic_workspace_token=survives_gpu_loss\n")
            .map_err(|e| e.to_string())?;
        let appearance = crate::session::PaneAppearance {
            title: Some("Grüße · 日本語 · العربية".into()),
            text_color: Some("#71DABC".into()),
            outline_color: Some("#C081E8".into()),
        };
        self.set_pane_appearance(window, pane, appearance.clone())?;
        {
            let state = self.windows.get_mut(&window).unwrap();
            state.palette_open = true;
            state.palette_target = Some((pane, session));
        }
        self.dispatch_palette(
            events,
            window,
            crate::palette_platform::PaletteAction::Submit {
                id: "pane.text-color".into(),
                value: "invalid".into(),
            },
        );
        check(
            self.windows[&window].pane.appearance == appearance,
            "invalid palette color changed pane",
        )?;
        self.dispatch_palette(
            events,
            window,
            crate::palette_platform::PaletteAction::Submit {
                id: "pane.title".into(),
                value: appearance.title.clone().unwrap(),
            },
        );
        check(!self.windows[&window].palette_open, "successful palette edit did not close")?;
        self.dispatch_palette(
            events,
            window,
            crate::palette_platform::PaletteAction::Submit {
                id: "loadout.save".into(),
                value: "late callback".into(),
            },
        );
        check(
            self.list_loadouts().map_err(|e| e.to_string())?.is_empty(),
            "late palette callback saved a loadout",
        )?;
        let split = self.split_pane(window, Axis::Vertical).ok_or("split workspace")?;
        {
            let state = self.windows.get_mut(&window).unwrap();
            let snapshot = state.tree.snapshot();
            check(state.toggle_pane_zoom(), "zoom did not activate")?;
            check(state.layout.panes.len() == 1, "zoom displayed hidden panes")?;
            state.focus_pane(pane);
            check(state.layout.panes[0].id == pane, "zoom did not follow focus")?;
            state.focus_pane(split);
            check(state.tree.snapshot() == snapshot, "zoom changed saved layout")?;
        }
        self.save_loadout("Multilingual work", true).map_err(|e| e.to_string())?;
        self.save_loadout("Layout only", false).map_err(|e| e.to_string())?;
        let original_sessions: Vec<_> = self
            .windows
            .values()
            .flat_map(|state| {
                state.tree.pane_ids().into_iter().map(|id| state.pane_state(id).unwrap().session)
            })
            .collect();
        let opened =
            self.open_loadout(events, "Multilingual work", true).map_err(|e| e.to_string())?;
        check(!opened.is_empty(), "loadout opened no windows")?;
        let restored = opened
            .iter()
            .filter_map(|id| self.windows.get(id))
            .find(|s| s.pane_state(pane).is_some_and(|p| p.appearance == appearance))
            .ok_or("loadout lost appearance")?;
        check(
            !restored.pane_zoomed && restored.layout.panes.len() == 2,
            "loadout persisted temporary zoom",
        )?;
        check(
            restored.pane_state(pane).unwrap().directory.as_ref() == Some(&launch),
            "loadout lost launch directory",
        )?;
        for old in &original_sessions {
            check(
                self.windows.values().any(|s| {
                    s.tree.pane_ids().iter().any(|id| s.pane_state(*id).unwrap().session == *old)
                }),
                "loadout replaced an existing shell",
            )?;
        }
        for id in opened {
            check(
                self.windows[&id].tree.pane_ids().iter().all(|p| {
                    !original_sessions.contains(&self.windows[&id].pane_state(*p).unwrap().session)
                }),
                "loadout reused session handles",
            )?;
        }
        let state = self.windows.get_mut(&window).unwrap();
        state.focus_pane(pane);
        state.pane.terminal.inject_local(b"\x1b[2J\x1b[HDefault \x1b[31mRed\x1b[0m");
        let theme = state.pane.theme(&self.config.theme);
        let grid = crate::convert::convert_grid(&state.pane.terminal, &theme, false);
        check(
            grid.get(0, 0).unwrap().fg == pane_color("#71DABC").unwrap(),
            "default foreground did not change",
        )?;
        check(
            grid.get(8, 0).unwrap().fg == self.config.theme.ansi.red,
            "pane foreground replaced ANSI color",
        )?;
        state.pane.search.active = true;
        state.pane.search.set_query("Default".into(), &state.pane.terminal);
        let search_match = state.pane.search.current();
        check(search_match.is_some(), "search fixture did not match")?;
        render_frame(state, &self.config, self.animations);
        state.renderer.simulate_device_loss();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !state.renderer.needs_device_recovery() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        check(state.renderer.needs_device_recovery(), "device destruction did not report loss")?;
        render_frame(state, &self.config, self.animations);
        check(!state.renderer.needs_device_recovery(), "GPU did not recover")?;
        check(
            state.pane.session == session && state.pane.appearance == appearance,
            "GPU recovery replaced pane state",
        )?;
        check(state.pane.search.active, "GPU recovery lost Find")?;
        check(
            state.pane.search.current() == search_match,
            "GPU recovery invalidated unchanged Find matches",
        )?;
        state
            .pane
            .terminal
            .write_to_pty(b"printf 'RECOVERED_%s\\n' \"$mechanic_workspace_token\"\n")
            .map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            state.pane.terminal.process_input();
            let text: String =
                state.pane.terminal.grid().display_iter().map(|cell| cell.cell.c).collect();
            if text.contains("RECOVERED_survives_gpu_loss") {
                break;
            }
            check(
                Instant::now() < deadline,
                "shell variable lost or PTY stalled after GPU recovery",
            )?;
            std::thread::sleep(Duration::from_millis(5));
        }
        self.delete_loadout("Multilingual work").map_err(|e| e.to_string())?;
        self.delete_loadout("Layout only").map_err(|e| e.to_string())?;
        check(
            self.list_loadouts().map_err(|e| e.to_string())?.is_empty(),
            "loadout delete left entries",
        )?;
        Ok(())
    }
}
