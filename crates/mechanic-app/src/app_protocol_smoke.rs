//! Opt-in native protocol checks; the system clipboard is never accessed.
use super::*;
use alacritty_terminal::vte::ansi::KeyboardModes;

pub(crate) struct ProtocolFixture {
    window: WindowId,
    pane: PaneId,
    session: u64,
}

fn check(value: bool, message: &str) -> Result<(), String> {
    value.then_some(()).ok_or_else(|| message.to_owned())
}

impl App {
    pub(crate) fn smoke_prepare_protocols(
        &mut self,
        events: &ActiveEventLoop,
    ) -> Result<ProtocolFixture, String> {
        let window = self.spawn_window(events, None).ok_or("protocol window creation failed")?;
        let pane = self.windows[&window].tree.active();
        let session = self.windows[&window].pane.session;
        crate::clipboard_platform::native_smoke(&self.windows[&window].window)?;
        let fixture = ProtocolFixture { window, pane, session };
        self.windows.get_mut(&window).unwrap().pane.terminal.write_to_pty(
            b"stty raw -echo; printf '\\033[>31uKEY_%s\\n' READY; dd bs=1 count=7 2>/dev/null | od -An -tx1; printf '\\033[<uKEY_%s\\n' DONE; stty sane\n"
        ).map_err(|e| e.to_string())?;
        self.smoke_wait_protocol_text(events, &fixture, "KEY_READY")?;
        let state = self.windows.get_mut(&window).unwrap();
        check(
            state.pane.terminal.keyboard_modes() == KeyboardModes::all(),
            "Kitty negotiation did not reach input",
        )?;
        let enter = Key::Named(NamedKey::Enter);
        let bytes = crate::input::translate_input_with_modes(
            crate::input::KeyInput {
                state: ElementState::Pressed,
                logical_key: &enter,
                unmodified_key: &enter,
                physical_key: PhysicalKey::Code(KeyCode::Enter),
                location: winit::keyboard::KeyLocation::Standard,
                text: Some("\r"),
                repeat: false,
            },
            ModifiersState::SHIFT,
            state.pane.terminal.cursor_app_mode(),
            state.pane.terminal.keyboard_modes(),
        )
        .ok_or("Shift Enter was not encoded")?;
        check(bytes == b"\x1b[13;2u", "Shift Enter bytes differ")?;
        state.pane.terminal.write_to_pty(&bytes).map_err(|e| e.to_string())?;
        self.smoke_wait_protocol_text(events, &fixture, "KEY_DONE")?;
        let state = &self.windows[&window];
        check(
            protocol_words(state.pane_state(pane).unwrap()).contains("1b 5b 31 33 3b 32 75"),
            "PTY received wrong Kitty input",
        )?;
        check(
            state.pane.terminal.keyboard_modes().is_empty(),
            "Kitty pop did not restore legacy input",
        )?;

        let expected = b"\x1b]52;p;cHJvdG9jb2wtc2VsZWN0aW9u\x07";
        let command = format!(
            "stty raw -echo; printf 'CLIP_%s\\n' READY; dd bs=1 count={} 2>/dev/null | od -An -tx1; printf 'CLIP_%s\\n' DONE; stty sane\n",
            expected.len()
        );
        self.windows
            .get_mut(&window)
            .unwrap()
            .pane
            .terminal
            .write_to_pty(command.as_bytes())
            .map_err(|e| e.to_string())?;
        self.smoke_wait_protocol_text(events, &fixture, "CLIP_READY")?;
        self.config.clipboard.read = mechanic_config::ClipboardPolicy::Allow;
        self.config.clipboard.write = mechanic_config::ClipboardPolicy::Allow;
        self.windows
            .get_mut(&window)
            .unwrap()
            .pane
            .terminal
            .inject_local(b"\x1b]52;p;cHJvdG9jb2wtc2VsZWN0aW9u\x07\x1b]52;p;?\x07");
        self.drain_clipboard_requests(window, pane, session, false);
        check(
            self.windows[&window].pane.primary_selection.as_deref() == Some("protocol-selection"),
            "OSC52 did not update pane-local selection",
        )?;
        self.smoke_wait_protocol_text(events, &fixture, "CLIP_DONE")?;
        check(
            protocol_words(self.windows[&window].pane_state(pane).unwrap()).contains(
                &expected.iter().map(|byte| format!("{byte:02x}")).collect::<Vec<_>>().join(" "),
            ),
            "OSC52 response did not reach PTY",
        )?;

        let other = self.split_pane(window, Axis::Vertical).ok_or("protocol split failed")?;
        let state = self.windows.get_mut(&window).unwrap();
        state.focus_pane(other);
        state.toggle_pane_zoom();
        state.load_pane(pane);
        state.pane.terminal.inject_local(b"\r\n\x1b[?2026hSYNC_AFTER_TIMEOUT\r\n");
        check(
            !protocol_text(&state.pane).contains("SYNC_AFTER_TIMEOUT"),
            "synchronized text displayed before deadline",
        )?;
        check(state.pane.terminal.sync_deadline().is_some(), "synchronized deadline absent")?;
        state.load_pane(other);
        state.frame_pacer.set_occluded(true);
        state.focus_redraw_frames = 0;
        let mut next = None;
        self.schedule_sync_deadlines(Instant::now(), &mut next);
        check(next.is_some(), "hidden synchronized pane did not schedule its deadline")?;
        Ok(fixture)
    }

    pub(crate) fn smoke_protocols_done(
        &mut self,
        fixture: &ProtocolFixture,
    ) -> Result<bool, String> {
        let state = self.windows.get(&fixture.window).ok_or("protocol window disappeared")?;
        let pane = state.pane_state(fixture.pane).ok_or("protocol pane disappeared")?;
        check(pane.session == fixture.session, "protocol session changed")?;
        if pane.terminal.sync_deadline().is_some() {
            return Ok(false);
        }
        check(
            protocol_text(pane).contains("SYNC_AFTER_TIMEOUT"),
            "timer did not expose hidden synchronized output",
        )?;
        check(
            state.pane_zoomed
                && state.tree.active() != fixture.pane
                && state.frame_pacer.is_occluded(),
            "timeout changed focus or pane visibility",
        )?;
        let mut next = None;
        self.schedule_sync_deadlines(Instant::now(), &mut next);
        check(next.is_none(), "completed synchronized update left an idle deadline")?;
        Ok(true)
    }

    fn smoke_wait_protocol_text(
        &mut self,
        events: &ActiveEventLoop,
        fixture: &ProtocolFixture,
        needle: &str,
    ) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.pending_parsers.enqueue((fixture.window, fixture.pane, fixture.session));
            self.pump_parser(events);
            let state = self.windows.get(&fixture.window).ok_or("protocol window closed")?;
            let pane = state.pane_state(fixture.pane).ok_or("protocol pane closed")?;
            if protocol_text(pane).contains(needle) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for {needle}; grid={:?}; modes={:?}",
                    protocol_text(pane),
                    pane.terminal.keyboard_modes()
                ));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

fn protocol_text(pane: &PaneState) -> String {
    pane.terminal.grid().display_iter().map(|cell| cell.cell.c).collect()
}

fn protocol_words(pane: &PaneState) -> String {
    protocol_text(pane).split_whitespace().collect::<Vec<_>>().join(" ")
}
